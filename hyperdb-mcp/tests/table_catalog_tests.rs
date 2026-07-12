// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for [`hyperdb_mcp::table_catalog`] and its integration with
//! [`HyperMcpServer`]'s lazy bootstrap + ingest/execute hooks.
//!
//! The module-level tests drive the catalog API directly against a fresh
//! [`Engine`]; the server-level tests verify the lazy bootstrap path and
//! the `--bare` opt-out by opening the workspace file twice (once through
//! a server, then again with a plain `Engine` to inspect the on-disk
//! state).

use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::error::ErrorCode;
use hyperdb_mcp::server::HyperMcpServer;
use hyperdb_mcp::table_catalog::{self, MetadataFields, TABLE_CATALOG_TABLE};
use tempfile::TempDir;

/// Build a fresh engine against a temp `.hyper` workspace file. Matches
/// the pattern used by `saved_queries_tests::workspace_engine` so the
/// module interop surface stays consistent.
/// Uses `new_no_daemon` to avoid interference from any daemon running
/// in parallel (e.g. from daemon_tests in the same `cargo test` run).
fn workspace_engine() -> (Engine, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let engine = Engine::new_no_daemon(Some(path.to_str().unwrap().into())).unwrap();
    (engine, dir)
}

/// `true` if `name` exists in the persistent attachment's `public`
/// schema. The catalog tests target the persistent DB because that's
/// where MCP-managed bookkeeping lives in the new model; user-data
/// tables only matter to these tests in the same scope.
fn table_exists(engine: &Engine, name: &str) -> bool {
    let sql = format!(
        "SELECT tablename FROM \"persistent\".pg_catalog.pg_tables \
         WHERE schemaname = 'public' AND tablename = '{}'",
        name.replace('\'', "''")
    );
    engine
        .execute_query_to_json(&sql)
        .is_ok_and(|rows| !rows.is_empty())
}

// --- Catalog module ---------------------------------------------------------

/// `ensure_exists` is idempotent — calling it twice should not error and
/// should leave exactly one catalog table in place.
#[test]
fn ensure_exists_is_idempotent() {
    let (engine, _dir) = workspace_engine();
    table_catalog::ensure_exists(&engine).unwrap();
    table_catalog::ensure_exists(&engine).unwrap();
    assert!(table_exists(&engine, TABLE_CATALOG_TABLE));
}

/// A fresh `upsert_stub` creates a row with `load_tool` / `row_count` /
/// `loaded_at` / `last_refreshed_at` populated and all prose fields
/// left `NULL`.
#[test]
fn upsert_stub_creates_row_with_null_prose_fields() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".widgets (id INT)")
        .unwrap();
    table_catalog::upsert_stub(
        &engine,
        "widgets",
        "load_file",
        Some(r#"{"source_path":"/tmp/widgets.csv"}"#),
        Some(42),
        true,
    )
    .unwrap();

    let entry = table_catalog::get(&engine, "widgets").unwrap().unwrap();
    assert_eq!(entry.table_name, "widgets");
    assert_eq!(entry.load_tool.as_deref(), Some("load_file"));
    assert_eq!(entry.row_count, Some(42));
    assert!(entry
        .load_params
        .as_deref()
        .unwrap()
        .contains("widgets.csv"));
    assert!(entry.source_url.is_none());
    assert!(entry.purpose.is_none());
}

/// Calling `upsert_stub` a second time must preserve prose fields written
/// via `set_metadata` — mechanical updates should not stomp human-entered
/// metadata.
#[test]
fn upsert_stub_preserves_prose_on_reload() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".widgets (id INT)")
        .unwrap();
    table_catalog::upsert_stub(&engine, "widgets", "load_file", None, Some(10), true).unwrap();
    table_catalog::set_metadata(
        &engine,
        "widgets",
        &MetadataFields {
            purpose: Some("test data".into()),
            source_url: Some("https://example.com/widgets".into()),
            ..Default::default()
        },
    )
    .unwrap();

    // Simulate a second load of the same file.
    table_catalog::upsert_stub(&engine, "widgets", "load_file", None, Some(25), true).unwrap();

    let entry = table_catalog::get(&engine, "widgets").unwrap().unwrap();
    assert_eq!(entry.row_count, Some(25), "row count should refresh");
    assert_eq!(
        entry.purpose.as_deref(),
        Some("test data"),
        "prose should survive a reload"
    );
    assert_eq!(
        entry.source_url.as_deref(),
        Some("https://example.com/widgets"),
    );
}

/// `bump_refresh=true` moves `last_refreshed_at` forward but keeps the
/// original `loaded_at` — this is how we tell the catalog "you were just
/// reloaded from the same source".
#[test]
fn upsert_stub_bump_refresh_updates_last_refreshed_not_loaded_at() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".widgets (id INT)")
        .unwrap();
    table_catalog::upsert_stub(&engine, "widgets", "load_file", None, Some(1), true).unwrap();
    let first = table_catalog::get(&engine, "widgets").unwrap().unwrap();

    // Sleep just long enough to see the timestamp tick.
    std::thread::sleep(std::time::Duration::from_millis(20));
    table_catalog::upsert_stub(&engine, "widgets", "load_file", None, Some(2), true).unwrap();
    let second = table_catalog::get(&engine, "widgets").unwrap().unwrap();

    assert_eq!(
        first.loaded_at, second.loaded_at,
        "loaded_at should be stable across reloads"
    );
    assert!(
        second.last_refreshed_at > first.last_refreshed_at,
        "last_refreshed_at should advance on bump_refresh=true"
    );
}

/// `set_metadata` writes prose fields and returns the refreshed entry.
/// Mechanical fields must not be affected.
#[test]
fn set_metadata_updates_prose_without_touching_mechanical_fields() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".widgets (id INT)")
        .unwrap();
    table_catalog::upsert_stub(&engine, "widgets", "load_file", None, Some(10), true).unwrap();
    let before = table_catalog::get(&engine, "widgets").unwrap().unwrap();

    let entry = table_catalog::set_metadata(
        &engine,
        "widgets",
        &MetadataFields {
            purpose: Some("answering demo questions".into()),
            notes: Some("refreshed weekly".into()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(entry.purpose.as_deref(), Some("answering demo questions"));
    assert_eq!(entry.notes.as_deref(), Some("refreshed weekly"));
    assert_eq!(entry.load_tool, before.load_tool);
    assert_eq!(entry.row_count, before.row_count);
    assert_eq!(entry.loaded_at, before.loaded_at);
}

/// Passing an empty string explicitly clears a prose field.
#[test]
fn set_metadata_empty_string_clears_field() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".widgets (id INT)")
        .unwrap();
    table_catalog::upsert_stub(&engine, "widgets", "load_file", None, Some(1), true).unwrap();
    table_catalog::set_metadata(
        &engine,
        "widgets",
        &MetadataFields {
            purpose: Some("to be cleared".into()),
            ..Default::default()
        },
    )
    .unwrap();

    let cleared = table_catalog::set_metadata(
        &engine,
        "widgets",
        &MetadataFields {
            purpose: Some(String::new()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(cleared.purpose.is_none());
}

/// With no fields set, the update is rejected rather than silently
/// no-op'ing — callers that meant to pass something shouldn't get a
/// misleading success.
#[test]
fn set_metadata_rejects_empty_payload() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".widgets (id INT)")
        .unwrap();
    table_catalog::upsert_stub(&engine, "widgets", "load_file", None, Some(1), true).unwrap();
    let err =
        table_catalog::set_metadata(&engine, "widgets", &MetadataFields::default()).unwrap_err();
    assert_eq!(err.code, ErrorCode::EmptyData);
}

/// `set_metadata` on an unstubbed table errors with `TableNotFound` — the
/// catalog must not accept prose metadata for a row the server hasn't
/// first stubbed.
#[test]
fn set_metadata_unknown_table_errors_with_table_not_found() {
    let (engine, _dir) = workspace_engine();
    table_catalog::ensure_exists(&engine).unwrap();
    let err = table_catalog::set_metadata(
        &engine,
        "no_such_table",
        &MetadataFields {
            purpose: Some("x".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::TableNotFound);
}

/// `reconcile` must stub rows for tables missing from the catalog, drop
/// rows whose table was deleted, and refresh `row_count` on existing
/// entries.
#[test]
fn reconcile_inserts_stubs_drops_orphans_refreshes_counts() {
    let (engine, _dir) = workspace_engine();

    // Three user tables exist; the catalog starts with an entry for a
    // table that was dropped, and only one of the three live tables.
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".alpha (id INT)")
        .unwrap();
    engine
        .execute_command("INSERT INTO \"persistent\".\"public\".alpha VALUES (1), (2)")
        .unwrap();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".beta (id INT)")
        .unwrap();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".gamma (id INT)")
        .unwrap();

    // Pre-populate catalog: known `alpha` (stale count) + orphan `zeta`.
    table_catalog::upsert_stub(&engine, "alpha", "load_file", None, Some(0), true).unwrap();
    table_catalog::upsert_stub(&engine, "zeta", "load_file", None, Some(100), true).unwrap();

    table_catalog::reconcile(&engine).unwrap();

    let all = table_catalog::list(&engine).unwrap();
    let names: Vec<_> = all.iter().map(|e| e.table_name.clone()).collect();
    assert!(names.contains(&"alpha".to_string()));
    assert!(names.contains(&"beta".to_string()));
    assert!(names.contains(&"gamma".to_string()));
    assert!(
        !names.contains(&"zeta".to_string()),
        "orphan must be dropped"
    );

    let alpha = all.iter().find(|e| e.table_name == "alpha").unwrap();
    assert_eq!(
        alpha.row_count,
        Some(2),
        "existing row count must be refreshed"
    );
    // `alpha` was already in the catalog → existing load_tool preserved.
    assert_eq!(alpha.load_tool.as_deref(), Some("load_file"));

    let beta = all.iter().find(|e| e.table_name == "beta").unwrap();
    assert_eq!(
        beta.load_tool.as_deref(),
        Some("unknown"),
        "newly discovered tables get load_tool='unknown'"
    );
}

/// `reconcile` does not track the catalog table itself or `_hyperdb_*`
/// infrastructure tables — those would pollute the user-facing catalog.
#[test]
fn reconcile_skips_internal_tables() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".alpha (id INT)")
        .unwrap();
    // Force-create a `_hyperdb_*` table the way `saved_queries` would.
    engine
        .execute_command("CREATE TABLE _hyperdb_saved_queries (name TEXT NOT NULL)")
        .unwrap();

    table_catalog::reconcile(&engine).unwrap();
    let entries = table_catalog::list(&engine).unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.table_name.clone()).collect();
    assert!(names.contains(&"alpha".to_string()));
    assert!(!names.contains(&TABLE_CATALOG_TABLE.to_string()));
    assert!(!names.contains(&"_hyperdb_saved_queries".to_string()));
}

/// `delete_for` removes the row and returns `true` the first time,
/// `false` on a second call.
#[test]
fn delete_for_is_idempotent() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".widgets (id INT)")
        .unwrap();
    table_catalog::upsert_stub(&engine, "widgets", "load_file", None, Some(1), true).unwrap();
    assert!(table_catalog::delete_for(&engine, "widgets").unwrap());
    assert!(!table_catalog::delete_for(&engine, "widgets").unwrap());
    assert!(table_catalog::get(&engine, "widgets").unwrap().is_none());
}

// --- HyperMcpServer integration --------------------------------------------
//
// Uses `with_no_daemon` / `new_no_daemon` to avoid interference from any
// daemon running in parallel (e.g. from daemon_tests in the same `cargo test`
// run). Without this, the server or engine may connect to a daemon left over
// from another test file, causing "database still in use" errors on Windows.

/// Default (non-bare) server: the catalog is created on first engine
/// use and survives across reopens of the workspace file.
#[test]
fn default_server_auto_creates_catalog_on_first_engine_use() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let path_str = path.to_str().unwrap().to_string();

    {
        let server = HyperMcpServer::with_no_daemon(Some(path_str.clone()), false, true);
        // Any tool that takes the engine lazily triggers bootstrap.
        let _ = server.resource_body_for_uri("hyper://workspace").unwrap();
    }

    let engine = Engine::new_no_daemon(Some(path_str)).unwrap();
    assert!(
        table_exists(&engine, TABLE_CATALOG_TABLE),
        "_table_catalog must be present in the workspace after a default server has touched it"
    );
}

/// Read-only mode must not attempt to create the catalog either — the
/// first tool call on a pristine workspace shouldn't turn around and
/// issue a `CREATE TABLE`.
#[test]
fn read_only_server_does_not_create_catalog() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let path_str = path.to_str().unwrap().to_string();

    // Seed the workspace with a data table so the server has something
    // to report; without this, `hyper://workspace` still runs fine, but
    // we want to make sure the reconciler doesn't fire either.
    {
        let engine = Engine::new_no_daemon(Some(path_str.clone())).unwrap();
        engine
            .execute_command("CREATE TABLE \"persistent\".\"public\".widgets (id INT)")
            .unwrap();
    }

    {
        let server = HyperMcpServer::with_no_daemon(Some(path_str.clone()), true, true);
        let _ = server.resource_body_for_uri("hyper://workspace").unwrap();
    }

    let engine = Engine::new_no_daemon(Some(path_str)).unwrap();
    assert!(
        !table_exists(&engine, TABLE_CATALOG_TABLE),
        "_table_catalog must NOT be created by a read-only server"
    );
}

/// Reopening a pre-existing workspace that already has data tables must
/// backfill the catalog with stub rows for each user table (simulates
/// the upgrade path for workspaces that predate the catalog feature).
#[test]
fn backfill_stubs_preexisting_tables_on_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let path_str = path.to_str().unwrap().to_string();

    // Seed with two user tables, no catalog.
    {
        let engine = Engine::new_no_daemon(Some(path_str.clone())).unwrap();
        engine
            .execute_command("CREATE TABLE \"persistent\".\"public\".alpha (id INT)")
            .unwrap();
        engine
            .execute_command("INSERT INTO \"persistent\".\"public\".alpha VALUES (1), (2), (3)")
            .unwrap();
        engine
            .execute_command("CREATE TABLE \"persistent\".\"public\".beta (id INT)")
            .unwrap();
    }

    {
        let server = HyperMcpServer::with_no_daemon(Some(path_str.clone()), false, true);
        let _ = server.resource_body_for_uri("hyper://workspace").unwrap();
    }

    let engine = Engine::new_no_daemon(Some(path_str)).unwrap();
    let entries = table_catalog::list(&engine).unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.table_name.clone()).collect();
    assert!(names.contains(&"alpha".to_string()));
    assert!(names.contains(&"beta".to_string()));
    let alpha = entries.iter().find(|e| e.table_name == "alpha").unwrap();
    assert_eq!(alpha.row_count, Some(3));
    assert_eq!(
        alpha.load_tool.as_deref(),
        Some("unknown"),
        "backfilled rows are tagged as unknown origin"
    );
}

#[test]
fn rename_table_preserves_catalog_metadata() {
    let tmp = TempDir::new().unwrap();
    let path_str = tmp
        .path()
        .join("workspace.hyper")
        .to_str()
        .unwrap()
        .to_string();

    let engine = Engine::new_no_daemon(Some(path_str.clone())).unwrap();

    // Create a table and load data so it has a non-zero row count.
    engine
        .execute_command(
            "CREATE TABLE \"persistent\".\"public\".population (id INT, name TEXT NOT NULL)",
        )
        .unwrap();
    engine
        .execute_command(
            "INSERT INTO \"persistent\".\"public\".population VALUES (1, 'Earth'), (2, 'Mars'), (3, 'Venus')"
        )
        .unwrap();

    // Stub the catalog entry (simulates what the server does after ingest).
    table_catalog::upsert_stub(
        &engine,
        "population",
        "load_file",
        Some("{\"path\":\"/tmp/population.csv\"}"),
        Some(3),
        false,
    )
    .unwrap();

    // Set prose metadata that should survive the rename.
    table_catalog::set_metadata(
        &engine,
        "population",
        &MetadataFields {
            source_url: Some("https://example.com/population".into()),
            purpose: Some("Test rename metadata preservation".into()),
            notes: Some("Refresh: curl ...".into()),
            ..Default::default()
        },
    )
    .unwrap();

    // Verify metadata is set before rename.
    let before = table_catalog::get(&engine, "population").unwrap().unwrap();
    assert_eq!(
        before.source_url.as_deref(),
        Some("https://example.com/population")
    );
    assert_eq!(
        before.purpose.as_deref(),
        Some("Test rename metadata preservation")
    );
    assert_eq!(before.notes.as_deref(), Some("Refresh: curl ..."));
    assert_eq!(before.load_tool.as_deref(), Some("load_file"));
    assert_eq!(before.row_count, Some(3));

    // Rename the table (same as a user running execute(sql=["ALTER TABLE..."]))
    engine
        .execute_command(
            "ALTER TABLE \"persistent\".\"public\".population RENAME TO owid_population",
        )
        .unwrap();

    // Run reconcile (same as after_execute_catalog_update does post-execute).
    table_catalog::reconcile(&engine).unwrap();

    // The old name should be gone.
    assert!(
        table_catalog::get(&engine, "population").unwrap().is_none(),
        "old table name should not exist in catalog after rename"
    );

    // The new name should exist with ALL the original metadata preserved.
    let after = table_catalog::get(&engine, "owid_population")
        .unwrap()
        .expect("renamed table should have a catalog entry");
    assert_eq!(
        after.source_url.as_deref(),
        Some("https://example.com/population"),
        "source_url must survive rename"
    );
    assert_eq!(
        after.purpose.as_deref(),
        Some("Test rename metadata preservation"),
        "purpose must survive rename"
    );
    assert_eq!(
        after.notes.as_deref(),
        Some("Refresh: curl ..."),
        "notes must survive rename"
    );
    assert_eq!(
        after.load_tool.as_deref(),
        Some("load_file"),
        "load_tool must survive rename"
    );
    assert_eq!(after.row_count, Some(3), "row_count must survive rename");
    // Both timestamps should be preserved — a rename is not a refresh.
    assert_eq!(
        after.loaded_at, before.loaded_at,
        "loaded_at should be anchored to the original load time, not the rename time"
    );
    assert_eq!(
        after.last_refreshed_at, before.last_refreshed_at,
        "last_refreshed_at should be anchored to the original load time, not bumped on rename"
    );
}

#[test]
fn set_metadata_data_url_roundtrip() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".widgets (id INT)")
        .unwrap();
    table_catalog::upsert_stub(&engine, "widgets", "load_file", None, Some(10), true).unwrap();

    let entry = table_catalog::set_metadata(
        &engine,
        "widgets",
        &MetadataFields {
            data_url: Some("https://example.com/widgets.csv?v=2".into()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        entry.data_url.as_deref(),
        Some("https://example.com/widgets.csv?v=2"),
        "data_url should round-trip through set_metadata"
    );

    // Read it back fresh to confirm persistence.
    let fresh = table_catalog::get(&engine, "widgets").unwrap().unwrap();
    assert_eq!(
        fresh.data_url.as_deref(),
        Some("https://example.com/widgets.csv?v=2")
    );

    // Clear it with an empty string.
    table_catalog::set_metadata(
        &engine,
        "widgets",
        &MetadataFields {
            data_url: Some(String::new()),
            ..Default::default()
        },
    )
    .unwrap();
    let cleared = table_catalog::get(&engine, "widgets").unwrap().unwrap();
    assert!(
        cleared.data_url.is_none(),
        "empty string should clear data_url"
    );
}

// --- #195: ephemeral-primary tables and the shared catalog ------------------
//
// Unqualified DDL lands on the ephemeral primary (schema_search_path is
// pinned to primary_db_name()), mirroring an `execute` tool call on the
// default workspace. The `_table_catalog` is a single store in the
// persistent attachment shared by BOTH databases, so reconcile must
// enumerate the union {persistent tables} ∪ {ephemeral-primary tables}.

/// #195 (as filed): a table created via `execute` on the ephemeral primary
/// must be stubbed by the post-execute reconcile so `set_metadata` succeeds.
/// RED before the fix: `set_metadata` returns `TABLE_NOT_FOUND` because
/// reconcile only ever enumerated the persistent attachment.
#[test]
fn set_metadata_finds_execute_created_ephemeral_primary_table() {
    let (engine, _dir) = workspace_engine();
    engine.execute_command("CREATE TABLE regr_195 (id INT)").unwrap();
    // Mirrors after_execute_catalog_update → reconcile_in(engine, None).
    table_catalog::reconcile(&engine).unwrap();

    let entry = table_catalog::set_metadata(
        &engine,
        "regr_195",
        &MetadataFields {
            purpose: Some("derived analysis table".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(entry.purpose.as_deref(), Some("derived analysis table"));
}

/// The delete-trap: a correctly-stubbed ephemeral-primary table (registered
/// via the working ingest path, with user prose) must survive an unrelated
/// later structural `execute`. RED before the fix: the row is silently reaped
/// because it is absent from the persistent-only live set.
#[test]
fn reconcile_preserves_ephemeral_primary_metadata_across_unrelated_ddl() {
    let (engine, _dir) = workspace_engine();
    engine.execute_command("CREATE TABLE trap_probe (id INT)").unwrap();
    engine
        .execute_command("INSERT INTO trap_probe VALUES (1), (2)")
        .unwrap();
    // load_data stubs the persistent catalog by explicit name (works today).
    table_catalog::upsert_stub(&engine, "trap_probe", "load_data", None, Some(2), true).unwrap();
    table_catalog::set_metadata(
        &engine,
        "trap_probe",
        &MetadataFields {
            purpose: Some("keep me".into()),
            source_url: Some("https://example.com/trap".into()),
            ..Default::default()
        },
    )
    .unwrap();

    // A later, unrelated structural execute on the primary triggers reconcile.
    engine.execute_command("CREATE TABLE trap_trigger (id INT)").unwrap();
    table_catalog::reconcile(&engine).unwrap();

    let entry = table_catalog::get(&engine, "trap_probe")
        .unwrap()
        .expect("trap_probe catalog row must survive an unrelated execute");
    assert_eq!(entry.purpose.as_deref(), Some("keep me"));
    assert_eq!(entry.source_url.as_deref(), Some("https://example.com/trap"));
}

/// Delete-trap guard: reconciling the shared catalog must not delete
/// persistent-table rows. Passes on current code (persistent tables are
/// already enumerated) — it guards against a naive fix that swaps
/// enumeration to ephemeral-only.
#[test]
fn reconcile_does_not_delete_persistent_rows_when_reconciling_shared_catalog() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".pers_tbl (id INT)")
        .unwrap();
    table_catalog::upsert_stub(&engine, "pers_tbl", "load_file", None, Some(0), true).unwrap();
    table_catalog::set_metadata(
        &engine,
        "pers_tbl",
        &MetadataFields {
            purpose: Some("persistent prose".into()),
            ..Default::default()
        },
    )
    .unwrap();
    // An ephemeral-primary table present alongside it.
    engine.execute_command("CREATE TABLE eph_tbl (id INT)").unwrap();

    table_catalog::reconcile(&engine).unwrap();

    let pers = table_catalog::get(&engine, "pers_tbl")
        .unwrap()
        .expect("persistent catalog row must survive reconcile");
    assert_eq!(pers.purpose.as_deref(), Some("persistent prose"));
}

/// No over-correction: a genuinely-dropped ephemeral-primary table's row
/// must still be reaped. Guards that the union fix does not turn into
/// never-reaping.
#[test]
fn reconcile_reaps_dropped_ephemeral_primary_table() {
    let (engine, _dir) = workspace_engine();
    engine.execute_command("CREATE TABLE dropme (id INT)").unwrap();
    table_catalog::upsert_stub(&engine, "dropme", "load_data", None, Some(5), true).unwrap();
    engine.execute_command("DROP TABLE dropme").unwrap();

    table_catalog::reconcile(&engine).unwrap();

    assert!(
        table_catalog::get(&engine, "dropme").unwrap().is_none(),
        "a genuinely dropped ephemeral-primary table's row must be reaped"
    );
}

/// C1 (deep-review Critical): the rename heuristic must NOT match a
/// disappeared PERSISTENT row against a new EPHEMERAL table on a coincident
/// row count — that would migrate persistent prose onto an unrelated scratch
/// table. RED today (scratch isn't stubbed at all → the `.expect` fails);
/// RED under a naive union (the false-rename fires → `purpose.is_none()`
/// fails); GREEN only when rename targets are restricted to persistent origin.
#[test]
fn reconcile_does_not_false_rename_persistent_row_onto_new_ephemeral_table() {
    let (engine, _dir) = workspace_engine();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".sales (id INT)")
        .unwrap();
    table_catalog::upsert_stub(&engine, "sales", "load_file", None, Some(0), true).unwrap();
    table_catalog::set_metadata(
        &engine,
        "sales",
        &MetadataFields {
            purpose: Some("quarterly sales".into()),
            ..Default::default()
        },
    )
    .unwrap();
    // A brand-new ephemeral-primary scratch table with a coincident count (0).
    engine.execute_command("CREATE TABLE scratch (id INT)").unwrap();
    // Drop the persistent table so `sales` becomes a "disappeared" catalog row.
    engine
        .execute_command("DROP TABLE \"persistent\".\"public\".sales")
        .unwrap();

    table_catalog::reconcile(&engine).unwrap();

    assert!(
        table_catalog::get(&engine, "sales").unwrap().is_none(),
        "disappeared persistent row must be deleted, not renamed onto an ephemeral table"
    );
    let scratch = table_catalog::get(&engine, "scratch")
        .unwrap()
        .expect("new ephemeral table must be stubbed");
    assert!(
        scratch.purpose.is_none(),
        "scratch must NOT inherit the persistent table's prose via a false rename"
    );
}

/// Name collision across DBs: the catalog is name-keyed (no origin column),
/// so when the same name exists in both the ephemeral primary and persistent,
/// the persistent (durable) table is authoritative for the count and the row
/// survives. Locks the persistent-wins insert order in the union.
#[test]
fn reconcile_persistent_wins_name_collision_across_dbs() {
    let (engine, _dir) = workspace_engine();
    engine.execute_command("CREATE TABLE dup (id INT)").unwrap();
    engine
        .execute_command("INSERT INTO dup VALUES (1), (2), (3)")
        .unwrap();
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".dup (id INT)")
        .unwrap();
    engine
        .execute_command(
            "INSERT INTO \"persistent\".\"public\".dup VALUES (1), (2), (3), (4), (5), (6), (7)",
        )
        .unwrap();
    table_catalog::upsert_stub(&engine, "dup", "load_file", None, Some(0), true).unwrap();

    table_catalog::reconcile(&engine).unwrap();

    let dup = table_catalog::get(&engine, "dup")
        .unwrap()
        .expect("collided catalog row must survive reconcile");
    assert_eq!(
        dup.row_count,
        Some(7),
        "persistent table (7 rows) must win the count over the ephemeral primary (3 rows)"
    );
}
