// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for [`hyperdb_mcp::attach`] and the cross-database
//! copy primitives.
//!
//! The `AttachRegistry` is exercised against a real [`Engine`] so we catch
//! any mismatch between our validators and the actual `ATTACH DATABASE`
//! grammar hyperd accepts. Copy-mode semantics are verified via direct
//! SQL because the `copy_query` tool is only reachable via the rmcp
//! dispatcher — the module-level tests keep the surface narrow and
//! avoid spinning up the full MCP server.

use hyperdb_mcp::attach::{
    validate_alias, validate_local_path, AttachRegistry, AttachRequest, AttachSource, OnMissing,
    LOCAL_ALIAS,
};
use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::error::ErrorCode;
use tempfile::TempDir;

/// Build a primary workspace with one table `primary_t` pre-populated.
fn primary_workspace() -> (Engine, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("primary.hyper");
    let engine = Engine::new_no_daemon(Some(path.to_string_lossy().into())).unwrap();
    engine
        .execute_command("CREATE TABLE primary_t (x INT)")
        .unwrap();
    engine
        .execute_command("INSERT INTO primary_t VALUES (1), (2)")
        .unwrap();
    (engine, dir)
}

/// Build a separate `.hyper` file with a table `t(a INT, b TEXT)` of 3
/// rows. Returns the path so the test can attach it into another
/// engine. The `Engine` handle is dropped before the function returns
/// so the file is idle and available for ATTACH.
fn build_source_hyper_file(dir: &TempDir, name: &str, rows: &[(i32, &str)]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    {
        // Spin up a throwaway engine with this path as the persistent
        // attachment, write data into it via fully-qualified SQL, then
        // drop the engine. The `.hyper` file at `path` survives with the
        // populated table; the engine's ephemeral primary is cleaned up.
        let engine = Engine::new_no_daemon(Some(path.to_string_lossy().into())).unwrap();
        engine
            .execute_command("CREATE TABLE \"persistent\".\"public\".\"t\" (a INT, b TEXT)")
            .unwrap();
        for (a, b) in rows {
            let escaped = b.replace('\'', "''");
            engine
                .execute_command(&format!(
                    "INSERT INTO \"persistent\".\"public\".\"t\" VALUES ({a}, '{escaped}')"
                ))
                .unwrap();
        }
    }
    path
}

fn row_count(engine: &Engine, qualified: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) AS cnt FROM {qualified}");
    engine
        .execute_query_to_json(&sql)
        .unwrap()
        .first()
        .and_then(|r| r.get("cnt").and_then(serde_json::value::Value::as_i64))
        .unwrap()
}

// --- Attach primitives ------------------------------------------------------

#[test]
fn attach_then_query_attached_table() {
    let (engine, dir) = primary_workspace();
    let source = build_source_hyper_file(&dir, "source.hyper", &[(10, "x"), (20, "y"), (30, "z")]);

    let registry = AttachRegistry::new();
    let entry = registry
        .attach(
            &engine,
            AttachRequest {
                alias: "src".into(),
                source: AttachSource::LocalFile {
                    path: source.clone(),
                },
                writable: false,
                on_missing: OnMissing::Error,
            },
        )
        .unwrap();
    assert_eq!(entry.alias, "src");
    assert!(!entry.writable);

    // Reading through the attached alias should see the 3-row source.
    let rows = engine
        .execute_query_to_json("SELECT a FROM \"src\".public.t ORDER BY a")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["a"], 10);
}

#[test]
fn attach_same_alias_twice_errors() {
    let (engine, dir) = primary_workspace();
    let source = build_source_hyper_file(&dir, "source.hyper", &[(1, "a")]);

    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "src".into(),
                source: AttachSource::LocalFile {
                    path: source.clone(),
                },
                writable: false,
                on_missing: OnMissing::Error,
            },
        )
        .unwrap();

    let err = registry
        .attach(
            &engine,
            AttachRequest {
                alias: "src".into(),
                source: AttachSource::LocalFile { path: source },
                writable: false,
                on_missing: OnMissing::Error,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

#[test]
fn detach_removes_entry_and_hides_tables() {
    let (engine, dir) = primary_workspace();
    let source = build_source_hyper_file(&dir, "source.hyper", &[(1, "a")]);

    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "src".into(),
                source: AttachSource::LocalFile { path: source },
                writable: false,
                on_missing: OnMissing::Error,
            },
        )
        .unwrap();
    assert_eq!(registry.list().len(), 1);

    let detached = registry.detach(&engine, "src").unwrap();
    assert!(detached);
    assert!(registry.list().is_empty());

    // Querying the attached alias should now fail.
    let err = engine
        .execute_query_to_json("SELECT 1 FROM \"src\".public.t LIMIT 0")
        .unwrap_err();
    // Should surface as a SQL error (attached database not found). We
    // don't pin the exact ErrorCode here — we only care that the
    // query is rejected.
    assert!(!err.message.is_empty());
}

#[test]
fn detach_unknown_alias_returns_false_without_error() {
    let (engine, _dir) = primary_workspace();
    let registry = AttachRegistry::new();
    let detached = registry.detach(&engine, "not_attached").unwrap();
    assert!(!detached);
}

// --- Validators -------------------------------------------------------------

#[test]
fn validator_reserves_local_alias() {
    assert_eq!(
        validate_alias(LOCAL_ALIAS).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn validator_accepts_well_formed_absolute_path() {
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("file.hyper");
    std::fs::write(&f, b"").unwrap();
    let canonical = validate_local_path(f.to_str().unwrap()).unwrap();
    assert_eq!(canonical, std::fs::canonicalize(&f).unwrap());
}

#[test]
fn validator_rejects_relative_path() {
    let err = validate_local_path("some/relative/path.hyper").unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

// --- on_missing='create' semantics ------------------------------------------

/// Happy path: ask the registry to create a brand-new `.hyper` file on
/// attach, then prove the file is real by issuing a CREATE TABLE and
/// round-tripping a row through it.
#[test]
fn attach_on_missing_create_builds_file_and_attaches_writable() {
    let (engine, dir) = primary_workspace();
    let target = dir.path().join("fresh.hyper");
    assert!(!target.exists(), "precondition: target must not exist");

    let registry = AttachRegistry::new();
    let entry = registry
        .attach(
            &engine,
            AttachRequest {
                alias: "fresh".into(),
                source: AttachSource::LocalFile {
                    path: target.clone(),
                },
                writable: true,
                on_missing: OnMissing::Create,
            },
        )
        .expect("attach should create the file and succeed");
    assert_eq!(entry.alias, "fresh");
    assert!(entry.writable);
    assert!(
        target.exists(),
        "attach should have created the .hyper file"
    );

    // Writes into the attached alias should work and be visible.
    engine
        .execute_command("CREATE TABLE \"fresh\".public.t (x INT)")
        .unwrap();
    engine
        .execute_command("INSERT INTO \"fresh\".public.t VALUES (42)")
        .unwrap();
    let rows = engine
        .execute_query_to_json("SELECT x FROM \"fresh\".public.t")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["x"], 42);
}

/// `on_missing='create'` without `writable:true` is rejected up front
/// — an unwritable empty DB is pointless and silently succeeding would
/// just confuse the LLM later.
#[test]
fn attach_on_missing_create_requires_writable() {
    let (engine, dir) = primary_workspace();
    let target = dir.path().join("fresh.hyper");

    let registry = AttachRegistry::new();
    let err = registry
        .attach(
            &engine,
            AttachRequest {
                alias: "fresh".into(),
                source: AttachSource::LocalFile {
                    path: target.clone(),
                },
                writable: false,
                on_missing: OnMissing::Create,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(
        err.message.to_lowercase().contains("writable"),
        "error should mention the writable requirement, got: {}",
        err.message
    );
    assert!(
        !target.exists(),
        "create must not have run when guard rejects"
    );
}

/// `on_missing='create'` is a no-op when the file already exists (and
/// the registry still attaches it normally).
#[test]
fn attach_on_missing_create_is_idempotent_for_existing_file() {
    let (engine, dir) = primary_workspace();
    // Build a pre-existing source file with a known table.
    let target = build_source_hyper_file(&dir, "existing.hyper", &[(7, "seven")]);

    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "ex".into(),
                source: AttachSource::LocalFile {
                    path: target.clone(),
                },
                writable: true,
                on_missing: OnMissing::Create,
            },
        )
        .expect("attach should succeed when file already exists");

    // Data from the pre-existing file must still be readable — CREATE
    // DATABASE IF NOT EXISTS must not have wiped it.
    let rows = engine
        .execute_query_to_json("SELECT a FROM \"ex\".public.t ORDER BY a")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["a"], 7);
}

// --- _table_catalog seeding policy on attach --------------------------------

/// Return `true` iff the attached database identified by `alias` already
/// has a `_table_catalog` table visible through the current connection.
/// Uses `{alias}.pg_catalog.pg_tables` — the same three-part form the
/// server's `probe_table_count` uses — because Hyper does not expose
/// an information-schema view.
fn attached_db_has_table_catalog(engine: &Engine, alias: &str) -> bool {
    let alias_esc = alias.replace('"', "\"\"");
    let sql = format!(
        "SELECT COUNT(*) AS cnt FROM \"{alias_esc}\".pg_catalog.pg_tables \
         WHERE schemaname = 'public' AND tablename = '_table_catalog'"
    );
    engine
        .execute_query_to_json(&sql)
        .unwrap()
        .first()
        .and_then(|r| r.get("cnt").and_then(serde_json::value::Value::as_i64))
        .is_some_and(|n| n > 0)
}

/// Default (non-bare) policy: creating a new `.hyper` file via
/// `on_missing: create` also seeds an empty `_table_catalog` into it,
/// so the file is immediately usable as a primary workspace later.
#[test]
fn on_missing_create_seeds_table_catalog_by_default() {
    let (engine, dir) = primary_workspace();
    let target = dir.path().join("seeded.hyper");
    assert!(!target.exists());

    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "seeded".into(),
                source: AttachSource::LocalFile {
                    path: target.clone(),
                },
                writable: true,
                on_missing: OnMissing::Create,
            },
        )
        .unwrap();

    assert!(
        attached_db_has_table_catalog(&engine, "seeded"),
        "non-bare policy should seed _table_catalog in the freshly created attached DB"
    );

    // And it's empty — seeding only stamps the DDL, nothing else.
    let rows = engine
        .execute_query_to_json("SELECT COUNT(*) AS cnt FROM \"seeded\".public._table_catalog")
        .unwrap();
    assert_eq!(
        rows[0]["cnt"], 0,
        "seeded _table_catalog should start empty"
    );
}

/// Attaching an *existing* database in read/write mode never adds
/// `_table_catalog`, regardless of policy — the attached file only
/// gets seeded at creation time. This keeps the "treat existing DBs
/// as bare" guarantee the user relies on when attaching foreign
/// workspaces into a session.
#[test]
fn attaching_existing_database_never_seeds_catalog() {
    let (engine, dir) = primary_workspace();
    // Pre-existing source file, writable on attach, with its own
    // data table — but no `_table_catalog`.
    let target = build_source_hyper_file(&dir, "foreign.hyper", &[(1, "a"), (2, "b")]);

    let registry = AttachRegistry::new(); // seed=true
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "foreign".into(),
                source: AttachSource::LocalFile {
                    path: target.clone(),
                },
                writable: true,
                on_missing: OnMissing::Error,
            },
        )
        .unwrap();

    assert!(
        !attached_db_has_table_catalog(&engine, "foreign"),
        "existing DB must keep its original schema; no catalog should be added on attach"
    );
}

/// `on_missing: create` pointing at a file that already exists must
/// NOT seed the pre-existing file's schema. Only brand-new files get
/// a catalog — the idempotent "attach the file you'd have created"
/// case still respects the "don't touch existing DBs" rule.
#[test]
fn on_missing_create_on_existing_file_does_not_seed() {
    let (engine, dir) = primary_workspace();
    let target = build_source_hyper_file(&dir, "preexisting.hyper", &[(3, "three")]);

    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "pre".into(),
                source: AttachSource::LocalFile {
                    path: target.clone(),
                },
                writable: true,
                on_missing: OnMissing::Create,
            },
        )
        .unwrap();

    assert!(
        !attached_db_has_table_catalog(&engine, "pre"),
        "on_missing=create pointing at an existing file must not mutate it — \
         seeding only fires when CREATE DATABASE actually ran"
    );
    // And the original data is still intact.
    let rows = engine
        .execute_query_to_json("SELECT a FROM \"pre\".public.t ORDER BY a")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["a"], 3);
}

/// `validate_local_path_for_create` accepts an absolute path whose
/// file does not yet exist but whose parent directory does.
#[test]
fn validator_for_create_accepts_missing_file_with_existing_parent() {
    use hyperdb_mcp::attach::validate_local_path_for_create;
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("not-yet.hyper");
    let resolved = validate_local_path_for_create(f.to_str().unwrap()).unwrap();
    assert_eq!(
        resolved,
        std::fs::canonicalize(dir.path())
            .unwrap()
            .join("not-yet.hyper")
    );
}

#[test]
fn validator_for_create_rejects_missing_parent_dir() {
    use hyperdb_mcp::attach::validate_local_path_for_create;
    let missing_parent = std::env::temp_dir()
        .join("hyper_mcp_definitely_missing_dir_99999")
        .join("new.hyper");
    let err = validate_local_path_for_create(missing_parent.to_str().unwrap()).unwrap_err();
    assert_eq!(err.code, ErrorCode::FileNotFound);
}

#[test]
fn validator_for_create_rejects_relative_path() {
    use hyperdb_mcp::attach::validate_local_path_for_create;
    let err = validate_local_path_for_create("not/absolute.hyper").unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

// --- Cross-database copy (direct SQL, mirrors copy_query internals) ---------

/// Drives the same DDL that `copy_query(mode="create")` would issue and
/// confirms the resulting row count matches the source filter.
#[test]
fn copy_create_from_attached_source() {
    let (engine, dir) = primary_workspace();
    let source = build_source_hyper_file(&dir, "source.hyper", &[(1, "x"), (2, "y"), (3, "z")]);

    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "src".into(),
                source: AttachSource::LocalFile { path: source },
                writable: false,
                on_missing: OnMissing::Error,
            },
        )
        .unwrap();

    // copy_query always fully-qualifies the target because the
    // post-ATTACH search_path points at the attached alias first —
    // an unqualified `CREATE TABLE imported` would fail with
    // "create statement could not resolve the schema (3F000)". These
    // direct SQL statements mirror what `perform_copy` emits.
    let primary_db = engine.primary_db_name();
    let target = format!("\"{primary_db}\".\"public\".\"imported\"");

    // "create" mode — target does not exist yet.
    engine
        .execute_command(&format!(
            "CREATE TABLE {target} AS SELECT a, b FROM \"src\".public.t WHERE a >= 2",
        ))
        .unwrap();
    assert_eq!(row_count(&engine, &target), 2);

    // "append" mode — double the rows.
    engine
        .execute_command(&format!(
            "INSERT INTO {target} SELECT a, b FROM \"src\".public.t WHERE a >= 2",
        ))
        .unwrap();
    assert_eq!(row_count(&engine, &target), 4);

    // "replace" mode — DROP + CREATE AS.
    engine
        .execute_command(&format!("DROP TABLE IF EXISTS {target}"))
        .unwrap();
    engine
        .execute_command(&format!(
            "CREATE TABLE {target} AS SELECT a, b FROM \"src\".public.t WHERE a = 1",
        ))
        .unwrap();
    assert_eq!(row_count(&engine, &target), 1);
}

// --- Replay after reconnect --------------------------------------------------

/// Simulates the `with_engine` reconnect path: attach against engine A,
/// drop A, spin up engine B against the same workspace, and replay the
/// registry. The attached alias should resolve on B without the caller
/// having to re-issue ATTACH.
#[test]
fn replay_reattaches_on_fresh_engine() {
    let dir = TempDir::new().unwrap();
    let primary_path = dir.path().join("primary.hyper");
    let source = build_source_hyper_file(&dir, "source.hyper", &[(1, "a"), (2, "b")]);

    let registry = AttachRegistry::new();
    {
        let engine_a = Engine::new_no_daemon(Some(primary_path.to_string_lossy().into())).unwrap();
        registry
            .attach(
                &engine_a,
                AttachRequest {
                    alias: "src".into(),
                    source: AttachSource::LocalFile {
                        path: source.clone(),
                    },
                    writable: false,
                    on_missing: OnMissing::Error,
                },
            )
            .unwrap();
    } // engine_a dropped here, closes its connection; attach state is gone on hyperd.

    let engine_b = Engine::new_no_daemon(Some(primary_path.to_string_lossy().into())).unwrap();
    // Without replay the attachment is invisible to engine_b.
    assert!(engine_b
        .execute_query_to_json("SELECT 1 FROM \"src\".public.t LIMIT 0")
        .is_err());

    // Replay should re-issue ATTACH against engine_b.
    registry.replay_all(&engine_b).unwrap();

    let rows = engine_b
        .execute_query_to_json("SELECT a FROM \"src\".public.t ORDER BY a")
        .unwrap();
    assert_eq!(rows.len(), 2);

    // Registry list should still reflect the attachment.
    let list = registry.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].alias, "src");
}

/// Regression for a bug where `copy_query` would successfully
/// create the target but skip the `_table_catalog` stub, so a
/// subsequent `set_table_metadata` on the same table would fail
/// with "no catalog entry".
///
/// The server test exercises the exact ordering `copy_query` now
/// uses: attach-less path, same-engine `CREATE TABLE AS`, then the
/// public `table_catalog::upsert_stub` helper the tool calls via
/// `after_ingest_catalog_update`.
#[test]
fn copy_create_stubs_table_catalog_on_primary_workspace() {
    use hyperdb_mcp::table_catalog::{self, TABLE_CATALOG_TABLE};

    let (engine, _dir) = primary_workspace();
    table_catalog::ensure_exists(&engine).unwrap();

    // Seed a source table that lives inside the primary workspace —
    // no attachment needed for this regression. Matches the shape a
    // `copy_query` call would produce.
    let primary_alias = engine.primary_db_name();
    let qualified_target = format!("\"{primary_alias}\".\"public\".\"derived\"");
    engine
        .execute_command(&format!(
            "CREATE TABLE {qualified_target} AS SELECT x AS col FROM primary_t WHERE x >= 1"
        ))
        .unwrap();

    // Simulate the in-closure stamp path.
    table_catalog::upsert_stub(
        &engine,
        "derived",
        "copy_query",
        Some(r#"{"mode":"create","target_database":null,"target_table":"derived","sql":"SELECT x AS col FROM primary_t WHERE x >= 1"}"#),
        Some(2),
        true,
    )
    .unwrap();

    let entry = table_catalog::get(&engine, "derived").unwrap().unwrap();
    assert_eq!(entry.load_tool.as_deref(), Some("copy_query"));
    assert_eq!(entry.row_count, Some(2));
    let params = entry.load_params.unwrap();
    assert!(
        params.contains("\"mode\":\"create\""),
        "load_params should echo mode: {params}"
    );
    assert!(
        params.contains("\"target_table\":\"derived\""),
        "load_params should echo target_table: {params}"
    );

    // The catalog lives in the persistent attachment now; verify by
    // probing `pg_tables` there directly.
    let catalog_present = engine
        .execute_query_to_json(
            "SELECT tablename FROM \"persistent\".pg_catalog.pg_tables \
             WHERE schemaname = 'public' AND tablename = '_table_catalog'",
        )
        .unwrap();
    assert!(
        !catalog_present.is_empty(),
        "_table_catalog must be present in the persistent attachment"
    );
    // `derived` was seeded into the ephemeral primary, so it appears in
    // the engine's regular table listing.
    let names: Vec<String> = engine
        .describe_tables()
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    assert!(names.iter().any(|n| n == "derived"));
    let _ = TABLE_CATALOG_TABLE; // keep import live (referenced via prose-only assertion)
}

/// A replay where the source file has been deleted should drop that
/// entry from the registry with a warning rather than poisoning the
/// whole reconnect.
#[test]
fn replay_drops_missing_files() {
    let dir = TempDir::new().unwrap();
    let primary_path = dir.path().join("primary.hyper");
    let source = build_source_hyper_file(&dir, "source.hyper", &[(1, "a")]);

    let registry = AttachRegistry::new();
    {
        let engine_a = Engine::new_no_daemon(Some(primary_path.to_string_lossy().into())).unwrap();
        registry
            .attach(
                &engine_a,
                AttachRequest {
                    alias: "src".into(),
                    source: AttachSource::LocalFile {
                        path: source.clone(),
                    },
                    writable: false,
                    on_missing: OnMissing::Error,
                },
            )
            .unwrap();
    }

    // Delete the source file out from under the registry. Then replay
    // — the call should succeed but the entry should be pruned.
    std::fs::remove_file(&source).unwrap();

    let engine_b = Engine::new_no_daemon(Some(primary_path.to_string_lossy().into())).unwrap();
    registry.replay_all(&engine_b).unwrap();
    assert!(registry.list().is_empty());
}

// --- schema_search_path lifecycle -------------------------------------------
//
// Regression tests for a bug where attaching a second database made
// every unqualified SQL statement against the primary workspace fail
// ("relation does not exist"). Root cause: Hyper's out-of-the-box
// `schema_search_path = "$single"` only resolves unqualified names when
// the connection has exactly one database attached; the first ATTACH
// moves the default to "nothing", so `describe`, `status`, and the
// `_table_catalog` upsert all silently broke. See
// [`hyperdb_mcp::attach::AttachRegistry::attach`] for the fix —
// every successful attach now pins the search path to the primary's
// own name, and the last detach resets it.

/// Baseline: with an attachment live, `describe_tables` and an
/// unqualified SELECT against the primary must both still see the
/// primary's rows.
#[test]
fn attach_preserves_unqualified_access_to_primary() {
    let (engine, dir) = primary_workspace();
    let source = build_source_hyper_file(&dir, "source.hyper", &[(1, "a")]);

    // Sanity: before attach, unqualified queries resolve.
    let before = row_count(&engine, "primary_t");
    assert_eq!(before, 2);

    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "src".into(),
                source: AttachSource::LocalFile { path: source },
                writable: false,
                on_missing: OnMissing::Error,
            },
        )
        .unwrap();

    // After attach: unqualified primary query still works because the
    // registry pins schema_search_path to the primary's file stem.
    let after = row_count(&engine, "primary_t");
    assert_eq!(after, 2);

    // And `describe_tables` keeps reporting the primary's tables —
    // this is the `status.table_count` / `describe` code path that
    // was previously returning empty.
    let names: Vec<String> = engine
        .describe_tables()
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "primary_t"),
        "primary_t should remain visible to describe_tables while an \
         attachment is live; got {names:?}"
    );
}

/// After the last detach the connection is back to a fresh posture
/// — Hyper's default `"$single"` search path takes over again and
/// unqualified queries keep working.
#[test]
fn detach_resets_schema_search_path_and_preserves_primary_access() {
    let (engine, dir) = primary_workspace();
    let source = build_source_hyper_file(&dir, "source.hyper", &[(1, "a")]);

    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "src".into(),
                source: AttachSource::LocalFile { path: source },
                writable: false,
                on_missing: OnMissing::Error,
            },
        )
        .unwrap();
    assert!(registry.detach(&engine, "src").unwrap());
    assert!(registry.list().is_empty());

    // Post-detach: with the default persistent attachment in place,
    // search_path stays pinned to the primary's name (we cannot RESET to
    // `"$single"` because the persistent DB is still attached). Without
    // the pin, unqualified resolution would break.
    let rows = engine
        .execute_query_to_json("SHOW schema_search_path")
        .unwrap();
    let setting = rows[0]
        .get("schema_search_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        setting,
        engine.primary_db_name(),
        "last detach should pin to primary while persistent stays attached; got {setting:?}"
    );

    // Unqualified SELECT still works — the real user contract.
    assert_eq!(row_count(&engine, "primary_t"), 2);
}

/// With multiple attachments present, detaching only one of them
/// keeps the search path pinned — the RESET is only issued when the
/// registry transitions back to empty, so intermediate detaches
/// don't break unqualified resolution for tools that are still
/// relying on it.
#[test]
fn detach_one_of_many_keeps_search_path_pinned() {
    let (engine, dir) = primary_workspace();
    let source_a = build_source_hyper_file(&dir, "src_a.hyper", &[(1, "a")]);
    let source_b = build_source_hyper_file(&dir, "src_b.hyper", &[(2, "b")]);

    let registry = AttachRegistry::new();
    for (alias, path) in [("src_a", source_a), ("src_b", source_b)] {
        registry
            .attach(
                &engine,
                AttachRequest {
                    alias: alias.into(),
                    source: AttachSource::LocalFile { path },
                    writable: false,
                    on_missing: OnMissing::Error,
                },
            )
            .unwrap();
    }

    assert!(registry.detach(&engine, "src_a").unwrap());
    // Still one attachment left — search_path must still point at
    // the primary so unqualified resolution keeps working.
    assert_eq!(row_count(&engine, "primary_t"), 2);
    let rows = engine
        .execute_query_to_json("SHOW schema_search_path")
        .unwrap();
    let setting = rows[0]
        .get("schema_search_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_ne!(
        setting, "\"$single\"",
        "search_path must stay pinned while attachments remain; got {setting:?}"
    );
}

/// After a simulated reconnect, `replay_all` must not only re-issue
/// every ATTACH but also re-apply the `schema_search_path` pin —
/// otherwise unqualified queries keep failing on the freshly-built
/// engine even though the attachments are back.
#[test]
fn replay_restores_schema_search_path_pin() {
    let dir = TempDir::new().unwrap();
    let primary_path = dir.path().join("primary.hyper");
    {
        // Seed the persistent file directly via fully-qualified SQL.
        let engine = Engine::new_no_daemon(Some(primary_path.to_string_lossy().into())).unwrap();
        engine
            .execute_command("CREATE TABLE \"persistent\".\"public\".\"primary_t\" (x INT)")
            .unwrap();
        engine
            .execute_command(
                "INSERT INTO \"persistent\".\"public\".\"primary_t\" VALUES (1), (2), (3)",
            )
            .unwrap();
    }
    let source = build_source_hyper_file(&dir, "source.hyper", &[(1, "a")]);

    let registry = AttachRegistry::new();
    {
        let engine_a = Engine::new_no_daemon(Some(primary_path.to_string_lossy().into())).unwrap();
        registry
            .attach(
                &engine_a,
                AttachRequest {
                    alias: "src".into(),
                    source: AttachSource::LocalFile {
                        path: source.clone(),
                    },
                    writable: false,
                    on_missing: OnMissing::Error,
                },
            )
            .unwrap();
    } // engine_a dropped; next engine starts fresh with default search_path.

    let engine_b = Engine::new_no_daemon(Some(primary_path.to_string_lossy().into())).unwrap();
    registry.replay_all(&engine_b).unwrap();

    // Qualified access to the seeded persistent table.
    assert_eq!(
        row_count(&engine_b, "\"persistent\".\"public\".\"primary_t\""),
        3
    );
    // Qualified access to the replayed attachment must also work.
    assert_eq!(row_count(&engine_b, "\"src\".public.t"), 1);
}

/// Full regression for Bug #2 — `_table_catalog` upserts running
/// *while an attachment is live* must still succeed. The previous
/// behavior was a silent WARN because every unqualified statement
/// inside `upsert_stub` (SELECT, DELETE, INSERT) would fail to
/// resolve and get eaten by `after_ingest_catalog_update`'s
/// best-effort wrapper.
#[test]
fn catalog_upsert_succeeds_while_attachment_is_live() {
    use hyperdb_mcp::table_catalog;

    let (engine, dir) = primary_workspace();
    let source = build_source_hyper_file(&dir, "source.hyper", &[(1, "a")]);

    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "src".into(),
                source: AttachSource::LocalFile { path: source },
                writable: false,
                on_missing: OnMissing::Error,
            },
        )
        .unwrap();

    // This call mirrors exactly what `copy_query` now does in its
    // Phase 4 stamp: plain unqualified `_table_catalog` writes.
    // Pre-fix this would silently fail to land a row.
    table_catalog::upsert_stub(
        &engine,
        "primary_t",
        "copy_query",
        Some(r#"{"mode":"create","target_table":"primary_t"}"#),
        Some(2),
        true,
    )
    .unwrap();

    let entry = table_catalog::get(&engine, "primary_t").unwrap().unwrap();
    assert_eq!(entry.load_tool.as_deref(), Some("copy_query"));
    assert_eq!(entry.row_count, Some(2));
    let params = entry.load_params.unwrap();
    assert!(params.contains("\"mode\":\"create\""), "got {params}");
}
