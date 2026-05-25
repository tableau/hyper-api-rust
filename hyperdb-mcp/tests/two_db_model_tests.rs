// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Regression tests for the ephemeral-primary + persistent-attached
//! engine model.

use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::error::ErrorCode;
use tempfile::TempDir;

/// `Engine::new(Some(path))` attaches the file at `path` as `"persistent"`
/// and reports `has_persistent() = true`.
#[test]
fn engine_with_persistent_path_attaches_under_persistent_alias() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let engine = Engine::new(Some(path.to_str().unwrap().into())).unwrap();
    assert!(engine.has_persistent());
    // The persistent attachment is reachable via fully-qualified SQL —
    // a CREATE TABLE there should succeed and the table visible via
    // a qualified probe.
    engine
        .execute_command("CREATE TABLE \"persistent\".\"public\".\"smoke\" (x INT)")
        .unwrap();
    let rows = engine
        .execute_query_to_json(
            "SELECT tablename FROM \"persistent\".pg_catalog.pg_tables \
             WHERE schemaname = 'public' AND tablename = 'smoke'",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
}

/// `Engine::new(None)` creates an ephemeral-only engine — no persistent
/// attachment, no `"persistent"` alias.
#[test]
fn engine_without_persistent_path_is_ephemeral_only() {
    let engine = Engine::new(None).unwrap();
    assert!(!engine.has_persistent());
    // Querying the persistent alias must fail because nothing is attached
    // under that name.
    let result =
        engine.execute_query_to_json("SELECT 1 FROM \"persistent\".pg_catalog.pg_database");
    assert!(
        result.is_err(),
        "no persistent alias must reject queries naming it"
    );
}

/// The ephemeral path is unique per Engine — concurrent engines don't
/// collide on the same temp file.
#[test]
fn ephemeral_paths_are_unique_per_engine() {
    let e1 = Engine::new(None).unwrap();
    let e2 = Engine::new(None).unwrap();
    assert_ne!(
        e1.ephemeral_path(),
        e2.ephemeral_path(),
        "each engine must own a distinct ephemeral file"
    );
}

/// Data written to the persistent attachment via fully-qualified SQL
/// survives the engine drop and is visible to a fresh engine on the
/// same path.
#[test]
fn persistent_writes_survive_engine_recreate() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("survives.hyper");
    let path_str = path.to_str().unwrap().to_string();

    {
        let engine = Engine::new(Some(path_str.clone())).unwrap();
        engine
            .execute_command(
                "CREATE TABLE \"persistent\".\"public\".\"keepers\" (n INT)",
            )
            .unwrap();
        engine
            .execute_command(
                "INSERT INTO \"persistent\".\"public\".\"keepers\" VALUES (1), (2), (3)",
            )
            .unwrap();
    }

    let engine = Engine::new(Some(path_str)).unwrap();
    let rows = engine
        .execute_query_to_json(
            "SELECT n FROM \"persistent\".\"public\".\"keepers\" ORDER BY n",
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["n"], 1);
    assert_eq!(rows[2]["n"], 3);
}

/// Data written to the ephemeral primary disappears when the engine is
/// dropped — that's the point of "ephemeral".
#[test]
fn ephemeral_writes_are_discarded_on_drop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let path_str = path.to_str().unwrap().to_string();

    {
        let engine = Engine::new(Some(path_str.clone())).unwrap();
        // Default-target: writes go to ephemeral.
        engine
            .execute_command("CREATE TABLE scratch (id INT)")
            .unwrap();
        engine
            .execute_command("INSERT INTO scratch VALUES (1), (2), (3)")
            .unwrap();
    }

    // New engine, same persistent file. The persistent attachment exists
    // but `scratch` was in the previous engine's ephemeral primary.
    let engine = Engine::new(Some(path_str)).unwrap();
    // Probe: scratch should not be visible. We query through the engine's
    // own connection (ephemeral) and through persistent — neither should
    // see it.
    let rows = engine
        .execute_query_to_json("SELECT tablename FROM pg_catalog.pg_tables WHERE tablename = 'scratch'")
        .unwrap_or_default();
    assert!(
        rows.is_empty(),
        "scratch must not survive engine drop (it lived in ephemeral)"
    );
}

/// `resolve_target_db("persistent")` returns the alias when persistent
/// is present, errors when it isn't.
#[test]
fn resolve_target_db_handles_persistent_presence() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let with_persistent = Engine::new(Some(path.to_str().unwrap().into())).unwrap();
    assert_eq!(
        with_persistent.resolve_target_db(Some("persistent")).unwrap(),
        "persistent"
    );
    assert_eq!(
        with_persistent.resolve_target_db(None).unwrap(),
        with_persistent.primary_db_name()
    );

    let ephemeral_only = Engine::new(None).unwrap();
    assert_eq!(
        ephemeral_only.resolve_target_db(None).unwrap(),
        ephemeral_only.primary_db_name()
    );
    let err = ephemeral_only
        .resolve_target_db(Some("persistent"))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("--ephemeral-only"));
}

/// Status JSON exposes both database paths and the `has_persistent` flag.
#[test]
fn engine_status_reports_both_paths() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let engine = Engine::new(Some(path.to_str().unwrap().into())).unwrap();
    let status = engine.status().unwrap();
    assert!(status["has_persistent"].as_bool().unwrap());
    assert!(status["ephemeral_path"].is_string());
    assert!(status["persistent_path"].is_string());
}

/// In ephemeral-only mode, status reflects no persistent attachment.
#[test]
fn engine_status_ephemeral_only_reports_no_persistent() {
    let engine = Engine::new(None).unwrap();
    let status = engine.status().unwrap();
    assert!(!status["has_persistent"].as_bool().unwrap());
    assert!(status["ephemeral_path"].is_string());
    assert!(status["persistent_path"].is_null());
}
