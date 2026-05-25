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

// Note on coverage: server-handler-level rejection paths (load_file
// merge+database, load_files+database, watch_directory+database,
// export format=hyper+database, attach-readonly+writable-required)
// are not reachable from this integration test layer without going
// through the rmcp tool router. The compile-time signatures of
// `create_table_async` / `build_parquet_ingest_sql` exercising the
// new `target_db` parameter cover the structural contract; the
// runtime rejection paths are covered by manual smoke tests and
// will land in a follow-up end-to-end MCP test harness.
