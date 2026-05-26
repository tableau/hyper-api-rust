// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the per-tool `database` parameter and `persist` flag.
//!
//! These tests exercise the lower-level routing primitives — the
//! `IngestOptions::target_db` field, fully-qualified SQL via
//! `qualified_table()`, `Engine::resolve_target_db`,
//! `Engine::scoped_search_path`, and the new `*_in` describe/sample
//! variants — that the MCP tool handlers thread the user's
//! `database` parameter into.

mod common;

use common::TestEngine;
use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::error::ErrorCode;
use hyperdb_mcp::ingest::{ingest_csv, ingest_json, qualified_table, IngestOptions};
use tempfile::TempDir;

// --- qualified_table helper -------------------------------------------------

/// `target_db: None` produces an unqualified `"table"` identifier so
/// existing single-DB call sites keep working unchanged.
#[test]
fn qualified_table_unqualified_when_target_db_none() {
    let opts = IngestOptions {
        table: "widgets".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    assert_eq!(qualified_table(&opts), "\"widgets\"");
}

/// `target_db: Some(db)` produces a fully-qualified
/// `"db"."public"."table"` identifier.
#[test]
fn qualified_table_qualifies_when_target_db_set() {
    let opts = IngestOptions {
        table: "widgets".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: Some("persistent".into()),
    };
    assert_eq!(
        qualified_table(&opts),
        "\"persistent\".\"public\".\"widgets\""
    );
}

/// Embedded double-quotes in the table name and database alias are
/// escaped per SQL identifier rules.
#[test]
fn qualified_table_escapes_embedded_quotes() {
    let opts = IngestOptions {
        table: "weird\"name".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: Some("db\"alias".into()),
    };
    assert_eq!(
        qualified_table(&opts),
        "\"db\"\"alias\".\"public\".\"weird\"\"name\""
    );
}

// --- Engine::resolve_target_db ---------------------------------------------

/// Empty / None / "local" all resolve to the primary db name. (LOCAL_ALIAS
/// filtering happens in the server-level `resolve_db` helper, which the
/// engine method doesn't see — so engine-level resolution returns "local"
/// verbatim. The server filter is exercised via end-to-end ingest below.)
#[test]
fn resolve_target_db_none_means_primary() {
    let te = TestEngine::new_ephemeral();
    let primary = te.engine.primary_db_name();
    assert_eq!(te.engine.resolve_target_db(None).unwrap(), primary);
    assert_eq!(te.engine.resolve_target_db(Some("")).unwrap(), primary);
    assert_eq!(
        te.engine.resolve_target_db(Some("   ")).unwrap(),
        primary,
        "whitespace-only treated as None"
    );
}

/// `"persistent"` resolves to itself when persistent storage is attached.
#[test]
fn resolve_target_db_persistent_when_attached() {
    let te = TestEngine::new_ephemeral();
    assert!(te.engine.has_persistent());
    assert_eq!(
        te.engine.resolve_target_db(Some("persistent")).unwrap(),
        "persistent"
    );
}

/// `"persistent"` returns `InvalidArgument` when the engine is in
/// ephemeral-only mode (no persistent attachment).
#[test]
fn resolve_target_db_persistent_errors_in_ephemeral_only() {
    let engine = Engine::new_no_daemon(None).expect("ephemeral engine");
    assert!(!engine.has_persistent());
    let err = engine
        .resolve_target_db(Some("persistent"))
        .expect_err("must reject persistent in ephemeral-only mode");
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("ephemeral-only"));
}

// --- Ingest with target_db -------------------------------------------------

/// `ingest_json` with `target_db: Some("persistent")` writes the table
/// into the persistent attachment, not the primary.
#[test]
fn ingest_json_with_persistent_target_lands_in_persistent() {
    let te = TestEngine::new_ephemeral();
    let opts = IngestOptions {
        table: "persisted_data".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: Some("persistent".into()),
    };
    let data = r#"[{"id": 1, "name": "Alice"}, {"id": 2, "name": "Bob"}]"#;
    let result = ingest_json(&te.engine, data, &opts).unwrap();
    assert_eq!(result.rows, 2);

    // Visible via fully-qualified SQL pointing at persistent.
    let rows = te
        .engine
        .execute_query_to_json(
            "SELECT * FROM \"persistent\".\"public\".\"persisted_data\" ORDER BY id",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "Alice");

    // NOT visible in the primary (no unqualified table named persisted_data).
    let primary_check = te
        .engine
        .execute_query_to_json("SELECT * FROM persisted_data");
    assert!(
        primary_check.is_err(),
        "table should not exist in the primary database"
    );
}

/// `ingest_csv` with `target_db: Some("persistent")` routes the COPY
/// FROM target table into the persistent attachment.
#[test]
fn ingest_csv_with_persistent_target_lands_in_persistent() {
    let te = TestEngine::new_ephemeral();
    let opts = IngestOptions {
        table: "csv_target".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: Some("persistent".into()),
    };
    let data = "id,name\n1,Alice\n2,Bob\n";
    let result = ingest_csv(&te.engine, data, &opts).unwrap();
    assert_eq!(result.rows, 2);

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM \"persistent\".\"public\".\"csv_target\" ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 2);
}

/// Persistent ingest survives engine drop and is visible to a fresh
/// engine on the same persistent path.
#[test]
fn ingest_to_persistent_survives_engine_recreate() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("survives.hyper");
    let path_str = path.to_str().unwrap().to_string();

    {
        let engine = Engine::new_no_daemon(Some(path_str.clone())).unwrap();
        let opts = IngestOptions {
            table: "library".into(),
            mode: "replace".into(),
            schema_override: None,
            merge_key: None,
            target_db: Some("persistent".into()),
        };
        let data = r#"[{"id": 1, "title": "Dune"}]"#;
        ingest_json(&engine, data, &opts).unwrap();
    }

    // Reopen and verify the table is still there.
    let engine = Engine::new_no_daemon(Some(path_str)).unwrap();
    let rows = engine
        .execute_query_to_json("SELECT title FROM \"persistent\".\"public\".\"library\"")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["title"], "Dune");
}

/// Default `target_db: None` ingest still lands in the primary
/// (ephemeral) database — backward-compat invariant.
#[test]
fn ingest_with_no_target_db_lands_in_primary() {
    let te = TestEngine::new_ephemeral();
    let opts = IngestOptions {
        table: "scratch".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let data = r#"[{"x": 1}]"#;
    ingest_json(&te.engine, data, &opts).unwrap();

    // Visible as unqualified table in the primary.
    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM scratch")
        .unwrap();
    assert_eq!(rows.len(), 1);
}

// --- describe_tables_in / sample_table_in ----------------------------------

/// `describe_tables_in(Some("persistent"))` lists tables in the
/// persistent attachment, hiding internal `_hyperdb_*` tables.
#[test]
fn describe_tables_in_persistent_lists_only_persistent_tables() {
    let te = TestEngine::new_ephemeral();
    // Create one table in primary and two in persistent.
    te.engine
        .execute_command("CREATE TABLE primary_only (x INT)")
        .unwrap();
    te.engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".\"alpha\" (x INT)")
        .unwrap();
    te.engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".\"beta\" (y TEXT)")
        .unwrap();

    let persistent_tables = te.engine.describe_tables_in(Some("persistent")).unwrap();
    let names: Vec<String> = persistent_tables
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    assert!(names.contains(&"alpha".into()));
    assert!(names.contains(&"beta".into()));
    assert!(
        !names.contains(&"primary_only".into()),
        "primary tables must not appear in persistent listing"
    );
}

/// `describe_table_in(Some("persistent"), name)` returns the schema of
/// a persistent table.
#[test]
fn describe_table_in_persistent_returns_schema() {
    let te = TestEngine::new_ephemeral();
    te.engine
        .execute_command(
            "CREATE TABLE \"persistent\".\"public\".\"orders\" (id INT, total NUMERIC(10,2))",
        )
        .unwrap();

    let info = te
        .engine
        .describe_table_in(Some("persistent"), "orders")
        .unwrap();
    assert_eq!(info.get("name").and_then(|v| v.as_str()), Some("orders"));
    let cols = info
        .get("columns")
        .and_then(|v| v.as_array())
        .expect("columns array");
    assert_eq!(cols.len(), 2);
}

/// `describe_table_in` errors with `TableNotFound` when the table
/// doesn't exist in the target database.
#[test]
fn describe_table_in_unknown_returns_table_not_found() {
    let te = TestEngine::new_ephemeral();
    let err = te
        .engine
        .describe_table_in(Some("persistent"), "no_such_table")
        .expect_err("missing table must error");
    assert_eq!(err.code, ErrorCode::TableNotFound);
}

/// `sample_table_in(Some("persistent"), ...)` returns rows from the
/// persistent table.
#[test]
fn sample_table_in_persistent_returns_rows() {
    let te = TestEngine::new_ephemeral();
    let opts = IngestOptions {
        table: "samples".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: Some("persistent".into()),
    };
    ingest_json(&te.engine, r#"[{"id": 1}, {"id": 2}, {"id": 3}]"#, &opts).unwrap();

    let sample = te
        .engine
        .sample_table_in(Some("persistent"), "samples", 5)
        .unwrap();
    assert_eq!(
        sample.get("table").and_then(|v| v.as_str()),
        Some("samples")
    );
    assert_eq!(
        sample.get("row_count").and_then(serde_json::Value::as_i64),
        Some(3)
    );
    let rows = sample
        .get("rows")
        .and_then(|v| v.as_array())
        .expect("rows array");
    assert_eq!(rows.len(), 3);
}

// --- ScopedSearchPath ------------------------------------------------------

/// `scoped_search_path` redirects unqualified SQL during the guard's
/// lifetime, then restores the primary on drop.
#[test]
fn scoped_search_path_redirects_and_restores() {
    let te = TestEngine::new_ephemeral();
    // Create a table in persistent and a different one in primary.
    te.engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".\"t\" (x INT)")
        .unwrap();
    te.engine
        .execute_command("INSERT INTO \"persistent\".\"public\".\"t\" VALUES (42)")
        .unwrap();
    te.engine.execute_command("CREATE TABLE t (x INT)").unwrap();
    te.engine
        .execute_command("INSERT INTO t VALUES (1), (2)")
        .unwrap();

    // Within the guard's scope, unqualified `SELECT * FROM t` must hit
    // the persistent copy.
    {
        let _guard = te.engine.scoped_search_path("persistent").unwrap();
        let rows = te
            .engine
            .execute_query_to_json("SELECT * FROM t ORDER BY x")
            .unwrap();
        assert_eq!(rows.len(), 1, "scoped to persistent's `t` (1 row)");
        assert_eq!(rows[0]["x"], 42);
    }

    // After drop, unqualified `SELECT * FROM t` must hit the primary again.
    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM t ORDER BY x")
        .unwrap();
    assert_eq!(rows.len(), 2, "search path restored to primary (2 rows)");
}

// --- Case-insensitive PERSISTENT_ALIAS matching ----------------------------

/// `"Persistent"`, `"PERSISTENT"`, `"persistent"` all resolve to the
/// canonical lowercase alias so the rest of the routing stack — quoted
/// SQL identifiers, attachment registry — sees a single form.
#[test]
fn resolve_target_db_persistent_is_case_insensitive() {
    let te = TestEngine::new_ephemeral();
    for variant in ["persistent", "Persistent", "PERSISTENT", "PerSiStEnT"] {
        let resolved = te
            .engine
            .resolve_target_db(Some(variant))
            .unwrap_or_else(|e| panic!("variant {variant:?} should resolve: {}", e.message));
        assert_eq!(
            resolved, "persistent",
            "variant {variant:?} must canonicalize to lowercase"
        );
    }
}

/// Case-insensitive matching also applies to the ephemeral-only error
/// path: `"PERSISTENT"` in ephemeral-only mode returns `InvalidArgument`,
/// not a confusing "database not found" later.
#[test]
fn resolve_target_db_persistent_uppercase_errors_in_ephemeral_only() {
    let engine = Engine::new_no_daemon(None).expect("ephemeral engine");
    let err = engine
        .resolve_target_db(Some("PERSISTENT"))
        .expect_err("uppercase persistent must reject too");
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

// --- Cross-database merge (Iter 1) -----------------------------------------

/// Merge into persistent when the target table doesn't exist yet:
/// the temp is renamed into the target slot. Verifies the rename path
/// works against a non-primary database.
#[test]
fn merge_into_persistent_creates_table_when_missing() {
    let te = TestEngine::new_ephemeral();
    let opts = IngestOptions {
        table: "merged_persist".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: Some("persistent".into()),
    };
    let data = r#"[{"id": 1, "name": "Alice"}, {"id": 2, "name": "Bob"}]"#;
    let result = ingest_json(&te.engine, data, &opts).unwrap();
    assert_eq!(result.rows, 2);
    assert!(
        result.stats.schema_changed,
        "newly-created target should report schema_changed=true"
    );

    let rows = te
        .engine
        .execute_query_to_json(
            "SELECT id, name FROM \"persistent\".\"public\".\"merged_persist\" ORDER BY id",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "Alice");
}

/// Merge into persistent when the target exists with overlapping keys:
/// matching rows are replaced, unmatched rows are appended.
#[test]
fn merge_into_persistent_replaces_matching_and_appends_new() {
    let te = TestEngine::new_ephemeral();
    // Seed target with two rows.
    let seed_opts = IngestOptions {
        table: "merge_target".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: Some("persistent".into()),
    };
    ingest_json(
        &te.engine,
        r#"[{"id": 1, "name": "old1"}, {"id": 2, "name": "old2"}]"#,
        &seed_opts,
    )
    .unwrap();

    // Merge: id=1 replaces, id=3 appends, id=2 stays.
    let merge_opts = IngestOptions {
        table: "merge_target".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: Some("persistent".into()),
    };
    let result = ingest_json(
        &te.engine,
        r#"[{"id": 1, "name": "new1"}, {"id": 3, "name": "new3"}]"#,
        &merge_opts,
    )
    .unwrap();
    assert_eq!(result.rows, 2, "INSERT row count from merge");

    let rows = te
        .engine
        .execute_query_to_json(
            "SELECT id, name FROM \"persistent\".\"public\".\"merge_target\" ORDER BY id",
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["name"], "new1", "id=1 replaced");
    assert_eq!(rows[1]["name"], "old2", "id=2 untouched");
    assert_eq!(rows[2]["name"], "new3", "id=3 appended");
}

/// Merge into persistent that introduces a new column triggers ALTER
/// TABLE ADD COLUMN against the persistent target, and the new column
/// shows up in the post-merge schema with NULL for pre-existing rows.
#[test]
fn merge_into_persistent_alters_when_incoming_has_new_column() {
    let te = TestEngine::new_ephemeral();
    let seed_opts = IngestOptions {
        table: "widens".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: Some("persistent".into()),
    };
    ingest_json(&te.engine, r#"[{"id": 1, "name": "Alice"}]"#, &seed_opts).unwrap();

    let merge_opts = IngestOptions {
        table: "widens".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: Some("persistent".into()),
    };
    let result = ingest_json(
        &te.engine,
        r#"[{"id": 2, "name": "Bob", "email": "bob@x.com"}]"#,
        &merge_opts,
    )
    .unwrap();
    assert!(
        result.stats.schema_changed,
        "new column 'email' should signal schema_changed"
    );

    let rows = te
        .engine
        .execute_query_to_json(
            "SELECT id, name, email FROM \"persistent\".\"public\".\"widens\" ORDER BY id",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows[0]["email"].is_null(),
        "pre-existing row gets NULL for new column"
    );
    assert_eq!(rows[1]["email"], "bob@x.com");
}

// --- Per-DB _table_catalog (Iter 4) ----------------------------------------

/// `ensure_exists_in(Some(alias))` creates `_table_catalog` inside the
/// user-attached database with the same schema as the persistent
/// catalog. Subsequent ingests can write to it via `upsert_stub_in`.
#[test]
fn ensure_exists_in_seeds_catalog_inside_user_attached_db() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "user_db".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    // Catalog doesn't exist yet in user_db (attach-only doesn't seed
    // for an existing file; this attached file was just created so
    // attach-with-on_missing=create *does* seed — verify and then
    // make a separate user-attach test below for the no-seed case).
    // Here we just confirm explicit ensure_exists_in is idempotent.
    hyperdb_mcp::table_catalog::ensure_exists_in(&engine, Some("user_db")).unwrap();
    hyperdb_mcp::table_catalog::ensure_exists_in(&engine, Some("user_db")).unwrap();

    // Probe via qualified pg_tables.
    let rows = engine
        .execute_query_to_json(
            "SELECT tablename FROM \"user_db\".pg_catalog.pg_tables \
             WHERE schemaname = 'public' AND tablename = '_table_catalog'",
        )
        .unwrap();
    assert_eq!(rows.len(), 1, "_table_catalog must exist in user_db");
}

/// `upsert_stub_in(target_db=Some("user_db"))` writes the row into the
/// user-attached database's catalog, NOT into the persistent catalog.
#[test]
fn upsert_stub_in_routes_to_user_attached_db() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "user_db".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    // Create a real table in user_db so the catalog row points to
    // something that exists.
    engine
        .execute_command("CREATE TABLE \"user_db\".\"public\".\"my_data\" (id INT)")
        .unwrap();

    hyperdb_mcp::table_catalog::upsert_stub_in(
        &engine,
        "my_data",
        "load_data",
        Some("{\"format\":\"json\"}"),
        Some(42),
        true,
        Some("user_db"),
        None,
    )
    .unwrap();

    // Visible via fully-qualified SELECT into user_db's catalog.
    let rows = engine
        .execute_query_to_json(
            "SELECT table_name, load_tool, row_count \
             FROM \"user_db\".\"public\".\"_table_catalog\" \
             WHERE table_name = 'my_data'",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["load_tool"], "load_data");
    assert_eq!(rows[0]["row_count"], 42);

    // NOT in the persistent catalog.
    let persistent_rows = engine
        .execute_query_to_json(
            "SELECT table_name FROM \"persistent\".\"public\".\"_table_catalog\" \
             WHERE table_name = 'my_data'",
        )
        .unwrap_or_default();
    assert!(
        persistent_rows.is_empty(),
        "the row must NOT bleed into persistent's catalog"
    );
}

/// `get_in(target_db=Some("user_db"))` reads from the user-attached
/// database's catalog and returns the row written by `upsert_stub_in`.
#[test]
fn get_in_reads_from_user_attached_db_catalog() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "user_db".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    engine
        .execute_command("CREATE TABLE \"user_db\".\"public\".\"events\" (id INT)")
        .unwrap();
    hyperdb_mcp::table_catalog::upsert_stub_in(
        &engine,
        "events",
        "load_file",
        None,
        Some(7),
        true,
        Some("user_db"),
        None,
    )
    .unwrap();

    let entry = hyperdb_mcp::table_catalog::get_in(&engine, "events", Some("user_db"))
        .unwrap()
        .expect("get_in must find the row written via upsert_stub_in");
    assert_eq!(entry.table_name, "events");
    assert_eq!(entry.row_count, Some(7));
    // Reading the persistent catalog for the same table_name returns
    // None — they are independent catalogs.
    let persistent_entry = hyperdb_mcp::table_catalog::get_in(&engine, "events", None).unwrap();
    assert!(persistent_entry.is_none());
}

/// `reconcile_in(target_db=Some("user_db"))` operates only on the
/// user-attached database's tables, leaving persistent's catalog
/// untouched.
#[test]
fn reconcile_in_per_db_does_not_touch_persistent_catalog() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "user_db".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    // Two tables in persistent (existing behavior).
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".\"persist_t\" (n INT)")
        .unwrap();
    // One table in user_db.
    engine
        .execute_command("CREATE TABLE \"user_db\".\"public\".\"user_t\" (n INT)")
        .unwrap();

    hyperdb_mcp::table_catalog::reconcile_in(&engine, Some("user_db")).unwrap();

    let user_rows = hyperdb_mcp::table_catalog::list_in(&engine, Some("user_db")).unwrap();
    let user_names: Vec<String> = user_rows.iter().map(|e| e.table_name.clone()).collect();
    assert!(
        user_names.contains(&"user_t".into()),
        "user_db reconcile must stub user_t; got {user_names:?}"
    );

    let persistent_rows = hyperdb_mcp::table_catalog::list_in(&engine, None).unwrap();
    let persistent_names: Vec<String> = persistent_rows
        .iter()
        .map(|e| e.table_name.clone())
        .collect();
    assert!(
        !persistent_names.contains(&"user_t".into()),
        "user_t must not appear in persistent's catalog; got {persistent_names:?}"
    );
}

// --- set_table_metadata + database (Iter 5) --------------------------------

/// `set_metadata_in(target_db=Some("user_db"))` updates prose fields
/// in the user-attached database's catalog after a stub row exists
/// there (the typical flow: load_data into the user DB seeds the
/// catalog automatically; set_table_metadata then updates prose).
#[test]
fn set_metadata_in_routes_to_user_attached_db() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};
    use hyperdb_mcp::table_catalog::MetadataFields;

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "user_db".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    // Stub a row first (set_metadata requires an existing entry).
    engine
        .execute_command("CREATE TABLE \"user_db\".\"public\".\"events\" (id INT)")
        .unwrap();
    hyperdb_mcp::table_catalog::upsert_stub_in(
        &engine,
        "events",
        "load_data",
        None,
        Some(0),
        true,
        Some("user_db"),
        None,
    )
    .unwrap();

    // Update prose via set_metadata_in.
    let fields = MetadataFields {
        source_url: Some("s3://bucket/events.parquet".into()),
        source_description: Some("Tracking events".into()),
        purpose: Some("daily reports".into()),
        license: None,
        notes: None,
    };
    let entry =
        hyperdb_mcp::table_catalog::set_metadata_in(&engine, "events", &fields, Some("user_db"))
            .unwrap();
    assert_eq!(
        entry.source_url.as_deref(),
        Some("s3://bucket/events.parquet")
    );
    assert_eq!(entry.purpose.as_deref(), Some("daily reports"));

    // Visible in the user_db catalog.
    let rows = engine
        .execute_query_to_json(
            "SELECT source_url, purpose FROM \"user_db\".\"public\".\"_table_catalog\" \
             WHERE table_name = 'events'",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["source_url"], "s3://bucket/events.parquet");
    assert_eq!(rows[0]["purpose"], "daily reports");
}

/// `set_metadata_in` errors with TableNotFound + a target-DB-named
/// message when the row is in a different DB than the caller asked
/// for. Specifically: row in persistent's catalog, ask user_db.
#[test]
fn set_metadata_in_missing_row_names_target_database_in_error() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};
    use hyperdb_mcp::table_catalog::MetadataFields;

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "user_db".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    // Stub in PERSISTENT, not user_db.
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".\"per_t\" (id INT)")
        .unwrap();
    hyperdb_mcp::table_catalog::upsert_stub_in(
        &engine,
        "per_t",
        "load_data",
        None,
        Some(0),
        true,
        None,
        None,
    )
    .unwrap();

    let fields = MetadataFields {
        source_url: Some("anything".into()),
        ..Default::default()
    };
    let err =
        hyperdb_mcp::table_catalog::set_metadata_in(&engine, "per_t", &fields, Some("user_db"))
            .expect_err("must error: row exists in persistent, not user_db");
    assert_eq!(err.code, ErrorCode::TableNotFound);
    assert!(
        err.message.contains("user_db"),
        "error must name the target db; got: {}",
        err.message
    );
}

// Note on coverage: server-handler-level rejection paths
// (load_files+database, watch_directory+database, export
// format=hyper+database, attach-readonly+writable-required,
// set_table_metadata.database with read-only target) are not
// reachable from this integration test layer without going through
// the rmcp tool router. They land in the end-to-end MCP test harness
// (Iter 6).

// =====================================================================
// M5 — alias canonicalization at attach time. The registry stores
// aliases lowercased, every lookup canonicalizes the input, and
// `Engine::resolve_target_db` lowercases non-persistent aliases so
// qualified SQL identifiers match the form used in `ATTACH DATABASE`.
// =====================================================================

/// Attaching with mixed case stores the lowercased form. `list()`
/// reflects the canonical form; `get()` accepts any case.
#[test]
fn attach_canonicalizes_alias_to_lowercase_at_attach_time() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "User_DB".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    let listed = reg.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].alias, "user_db",
        "attach must store the alias in lowercase"
    );

    // get() canonicalizes its input, so any casing finds the entry.
    assert!(reg.get("user_db").is_some(), "lowercase get must find");
    assert!(reg.get("User_DB").is_some(), "mixed-case get must find");
    assert!(reg.get("USER_DB").is_some(), "uppercase get must find");
}

/// Detach accepts any casing — pre-M5 it returned `Ok(false)` when the
/// case didn't match the stored form, leaving the catalog cache stale.
#[test]
fn detach_after_canonicalization_is_case_insensitive() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "User_DB".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    let detached = reg.detach(&engine, "user_db").unwrap();
    assert!(detached, "detach with lowercase must succeed");
    assert!(reg.list().is_empty());
}

/// After a mixed-case attach, the per-DB catalog is reachable via the
/// canonical (lowercase) qualified SQL form. `Engine::resolve_target_db`
/// now lowercases non-persistent aliases so the SQL identifier always
/// matches the ATTACH form, which the registry lowercases.
#[test]
fn qualified_catalog_in_uses_canonical_alias_after_attach() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "User_DB".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    // The mixed-case input survives `resolve_target_db` as the
    // canonical lowercase form, so qualified writes target the
    // lowercase database name that ATTACH actually used.
    let resolved = engine.resolve_target_db(Some("User_DB")).unwrap();
    assert_eq!(resolved, "user_db");

    // Direct qualified SELECT against the lowercase form must find the
    // catalog seeded by attach-on-create.
    let rows = engine
        .execute_query_to_json(
            "SELECT tablename FROM \"user_db\".pg_catalog.pg_tables \
             WHERE schemaname = 'public' AND tablename = '_table_catalog'",
        )
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "_table_catalog must exist under the canonical (lowercase) alias"
    );

    // Writing through `upsert_stub_in` with the user-typed mixed-case
    // alias must land in the same canonical catalog. This is the
    // round-trip that pre-M5 silently broke when the registry
    // mismatched the qualified-SQL form.
    engine
        .execute_command("CREATE TABLE \"user_db\".\"public\".\"my_t\" (id INT)")
        .unwrap();
    hyperdb_mcp::table_catalog::upsert_stub_in(
        &engine,
        "my_t",
        "load_data",
        None,
        Some(0),
        true,
        Some("User_DB"),
        None,
    )
    .unwrap();
    let rows = engine
        .execute_query_to_json(
            "SELECT table_name FROM \"user_db\".\"public\".\"_table_catalog\" \
             WHERE table_name = 'my_t'",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
}

// =====================================================================
// M4 — `execute` reconciles the user-attached target's catalog as well
// as persistent's. Pre-fix, raw DDL like `DROP TABLE` against a
// user-attached alias left the dropped table's row stranded in that
// DB's `_table_catalog` indefinitely. Bootstrap reconcile only walks
// persistent.
// =====================================================================

/// Simulates the post-fix `after_execute_catalog_update` flow: after a
/// DROP TABLE in a user-attached writable DB, both reconciles run and
/// the dropped table's stub row disappears from that DB's catalog.
/// Persistent's catalog stays untouched (no row was ever there).
#[test]
fn execute_drop_table_in_user_attached_reconciles_that_dbs_catalog() {
    use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};

    let dir = TempDir::new().unwrap();
    let primary = dir.path().join("primary.hyper");
    let attached = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary.to_string_lossy().into())).unwrap();
    let reg = AttachRegistry::new();
    reg.attach(
        &engine,
        AttachRequest {
            alias: "user_db".into(),
            source: AttachSource::LocalFile { path: attached },
            writable: true,
            on_missing: OnMissing::Create,
        },
    )
    .unwrap();

    // Seed a stub row in user_db's catalog (mimics what load_data
    // against the user-attached DB does).
    engine
        .execute_command("CREATE TABLE \"user_db\".\"public\".\"to_drop\" (id INT)")
        .unwrap();
    hyperdb_mcp::table_catalog::upsert_stub_in(
        &engine,
        "to_drop",
        "load_data",
        None,
        Some(0),
        true,
        Some("user_db"),
        None,
    )
    .unwrap();

    // Confirm the row landed.
    let before = engine
        .execute_query_to_json(
            "SELECT table_name FROM \"user_db\".\"public\".\"_table_catalog\" \
             WHERE table_name = 'to_drop'",
        )
        .unwrap();
    assert_eq!(before.len(), 1, "stub must exist before DROP");

    // Now drop the table — the user's `execute("DROP TABLE …")` would
    // run this exact SQL.
    engine
        .execute_command("DROP TABLE \"user_db\".\"public\".\"to_drop\"")
        .unwrap();

    // Mirror `after_execute_catalog_update`'s post-fix behavior: it
    // calls reconcile for persistent, then for the user target.
    hyperdb_mcp::table_catalog::reconcile_in(&engine, None).unwrap();
    hyperdb_mcp::table_catalog::reconcile_in(&engine, Some("user_db")).unwrap();

    // The stranded row in user_db's catalog must be gone.
    let after = engine
        .execute_query_to_json(
            "SELECT table_name FROM \"user_db\".\"public\".\"_table_catalog\" \
             WHERE table_name = 'to_drop'",
        )
        .unwrap();
    assert_eq!(
        after.len(),
        0,
        "post-execute reconcile must drop catalog rows for tables removed by raw DDL in the user-attached DB"
    );

    // And persistent's catalog never had a row for `to_drop` to begin
    // with; reconcile_in(None) must not have created one as a side
    // effect.
    let in_persistent = engine
        .execute_query_to_json(
            "SELECT table_name FROM \"persistent\".\"public\".\"_table_catalog\" \
             WHERE table_name = 'to_drop'",
        )
        .unwrap();
    assert_eq!(in_persistent.len(), 0);
}
