// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Pre-flight smoke battery for cross-database SQL shapes that the
//! "remove v1 limitations" PR plumbs through ingest/merge/export.
//!
//! Plan: `HYPERDB_MCP_REMOVE_V1_LIMITATIONS_PLAN.md` §"Pre-flight smoke
//! battery". Each test verifies one SQL shape against a *user-attached
//! writable* database. If any shape fails, the iter-1 merge-in-target-DB
//! design is dead and the plan needs to redesign before plumbing.
//!
//! Marked `#[ignore]` so they don't run on every `cargo test` — invoke
//! explicitly via `cargo test -p hyperdb-mcp --test cross_db_dml_smoke
//! -- --ignored`.

use hyperdb_mcp::attach::{AttachRegistry, AttachRequest, AttachSource, OnMissing};
use hyperdb_mcp::engine::Engine;
use tempfile::TempDir;

/// Build a primary workspace plus an attached writable database under
/// alias `"smoke"` pointing at a freshly-created `.hyper` file. Returns
/// `(engine, registry, _dir)` — drop order matters: registry → engine →
/// dir, so the temp directory outlives the engine.
fn setup() -> (Engine, AttachRegistry, TempDir) {
    let dir = TempDir::new().unwrap();
    let primary_path = dir.path().join("primary.hyper");
    let attached_path = dir.path().join("attached.hyper");

    let engine = Engine::new_no_daemon(Some(primary_path.to_string_lossy().into())).unwrap();
    let registry = AttachRegistry::new();
    registry
        .attach(
            &engine,
            AttachRequest {
                alias: "smoke".into(),
                source: AttachSource::LocalFile {
                    path: attached_path,
                },
                writable: true,
                on_missing: OnMissing::Create,
            },
        )
        .unwrap();
    (engine, registry, dir)
}

/// Shape 1: `CREATE TABLE "alias"."public"."tmp" AS SELECT ...` — the
/// merge path's temp-table strategy depends on this.
#[test]
#[ignore = "pre-flight smoke; run with --ignored"]
fn cross_db_ctas_into_attached_writable() {
    let (engine, _reg, _dir) = setup();
    engine
        .execute_command(
            "CREATE TABLE \"smoke\".\"public\".\"tmp\" AS \
             SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v(k, val)",
        )
        .expect("CTAS into attached writable DB must succeed");

    let rows = engine
        .execute_query_to_json("SELECT k, val FROM \"smoke\".\"public\".\"tmp\" ORDER BY k")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["k"], 1);
    assert_eq!(rows[1]["val"], "b");
}

/// Shape 2: qualified `DELETE ... USING ...` where target and source are
/// both in the attached DB — the merge path's row-by-key delete step.
#[test]
#[ignore = "pre-flight smoke; run with --ignored"]
fn cross_db_qualified_delete_using() {
    let (engine, _reg, _dir) = setup();

    engine
        .execute_command("CREATE TABLE \"smoke\".\"public\".\"t\" (k INT, v TEXT)")
        .unwrap();
    engine
        .execute_command(
            "INSERT INTO \"smoke\".\"public\".\"t\" VALUES (1, 'old'), (2, 'keep'), (3, 'old')",
        )
        .unwrap();

    engine
        .execute_command(
            "CREATE TABLE \"smoke\".\"public\".\"tmp\" AS \
             SELECT * FROM (VALUES (1), (3)) AS v(k)",
        )
        .unwrap();

    engine
        .execute_command(
            "DELETE FROM \"smoke\".\"public\".\"t\" t \
             USING \"smoke\".\"public\".\"tmp\" s \
             WHERE t.k = s.k",
        )
        .expect("qualified DELETE-USING must succeed across same attached DB");

    let rows = engine
        .execute_query_to_json("SELECT k, v FROM \"smoke\".\"public\".\"t\" ORDER BY k")
        .unwrap();
    assert_eq!(rows.len(), 1, "rows 1 and 3 should be deleted");
    assert_eq!(rows[0]["v"], "keep");
}

/// Shape 3: qualified `INSERT ... SELECT` — the merge path's append step
/// after the delete.
#[test]
#[ignore = "pre-flight smoke; run with --ignored"]
fn cross_db_qualified_insert_select() {
    let (engine, _reg, _dir) = setup();

    engine
        .execute_command("CREATE TABLE \"smoke\".\"public\".\"t\" (k INT, v TEXT)")
        .unwrap();
    engine
        .execute_command(
            "CREATE TABLE \"smoke\".\"public\".\"tmp\" AS \
             SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v(k, v)",
        )
        .unwrap();

    engine
        .execute_command(
            "INSERT INTO \"smoke\".\"public\".\"t\" \
             SELECT * FROM \"smoke\".\"public\".\"tmp\"",
        )
        .expect("qualified INSERT-SELECT must succeed");

    let rows = engine
        .execute_query_to_json("SELECT k, v FROM \"smoke\".\"public\".\"t\" ORDER BY k")
        .unwrap();
    assert_eq!(rows.len(), 2);
}

/// Shape 4: qualified `ALTER TABLE ... ADD COLUMN` — the merge path's
/// new-column promotion step when the incoming schema is wider.
#[test]
#[ignore = "pre-flight smoke; run with --ignored"]
fn cross_db_qualified_alter_add_column() {
    let (engine, _reg, _dir) = setup();

    engine
        .execute_command("CREATE TABLE \"smoke\".\"public\".\"t\" (k INT)")
        .unwrap();

    engine
        .execute_command("ALTER TABLE \"smoke\".\"public\".\"t\" ADD COLUMN extra TEXT")
        .expect("qualified ALTER ADD COLUMN must succeed");

    engine
        .execute_command("INSERT INTO \"smoke\".\"public\".\"t\" VALUES (1, 'x')")
        .unwrap();
    let rows = engine
        .execute_query_to_json("SELECT extra FROM \"smoke\".\"public\".\"t\"")
        .unwrap();
    assert_eq!(rows[0]["extra"], "x");
}

/// Shape 5: qualified `pg_catalog.pg_tables` and column-introspection
/// probes against the attached DB — the basis for `table_exists_in` and
/// `column_metadata_in`.
#[test]
#[ignore = "pre-flight smoke; run with --ignored"]
fn cross_db_qualified_pg_catalog_probes() {
    let (engine, _reg, _dir) = setup();

    engine
        .execute_command("CREATE TABLE \"smoke\".\"public\".\"probe_me\" (id INT, label TEXT)")
        .unwrap();

    let table_rows = engine
        .execute_query_to_json(
            "SELECT tablename FROM \"smoke\".pg_catalog.pg_tables \
             WHERE schemaname = 'public' AND tablename = 'probe_me'",
        )
        .expect("qualified pg_tables probe must succeed");
    assert_eq!(
        table_rows.len(),
        1,
        "table 'probe_me' must be visible via attached pg_catalog"
    );

    let column_rows = engine
        .execute_query_to_json(
            "SELECT a.attname AS name, t.typname AS type \
             FROM \"smoke\".pg_catalog.pg_attribute a \
             JOIN \"smoke\".pg_catalog.pg_class c ON a.attrelid = c.oid \
             JOIN \"smoke\".pg_catalog.pg_type t ON a.atttypid = t.oid \
             JOIN \"smoke\".pg_catalog.pg_namespace n ON c.relnamespace = n.oid \
             WHERE n.nspname = 'public' AND c.relname = 'probe_me' AND a.attnum > 0 \
             ORDER BY a.attnum",
        )
        .expect("qualified pg_attribute/pg_type join must succeed");
    assert_eq!(column_rows.len(), 2);
    assert_eq!(column_rows[0]["name"], "id");
    assert_eq!(column_rows[1]["name"], "label");
}
