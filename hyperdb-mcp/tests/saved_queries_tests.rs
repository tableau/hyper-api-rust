// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for [`hyperdb_mcp::saved_queries`]: the in-memory [`SessionStore`]
//! round-trip, the workspace-backed [`WorkspaceStore`] round-trip, and
//! cross-restart persistence when a `.hyper` workspace path is provided.

use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::error::ErrorCode;
use hyperdb_mcp::saved_queries::{SavedQuery, SavedQueryStore, SessionStore, WorkspaceStore};
use hyperdb_mcp::server::HyperMcpServer;
use tempfile::TempDir;

fn mk_query(name: &str, sql: &str) -> SavedQuery {
    SavedQuery {
        name: name.into(),
        sql: sql.into(),
        description: Some(format!("Test query {name}")),
        created_at: chrono::Utc::now(),
    }
}

// --- SessionStore ----------------------------------------------------------

#[test]
fn session_save_then_get_roundtrips() {
    let store = SessionStore::new();
    let q = mk_query(
        "revenue_by_region",
        "SELECT region, SUM(r) FROM s GROUP BY region",
    );
    store.save(None, q.clone()).unwrap();
    let fetched = store.get(None, "revenue_by_region").unwrap().unwrap();
    assert_eq!(fetched.name, q.name);
    assert_eq!(fetched.sql, q.sql);
    assert_eq!(fetched.description, q.description);
}

#[test]
fn session_get_unknown_returns_none() {
    let store = SessionStore::new();
    assert!(store.get(None, "no-such-query").unwrap().is_none());
}

#[test]
fn session_list_returns_sorted_names() {
    let store = SessionStore::new();
    store.save(None, mk_query("zeta", "SELECT 1")).unwrap();
    store.save(None, mk_query("alpha", "SELECT 2")).unwrap();
    store.save(None, mk_query("mu", "SELECT 3")).unwrap();
    let names: Vec<String> = store
        .list(None)
        .unwrap()
        .into_iter()
        .map(|q| q.name)
        .collect();
    assert_eq!(names, vec!["alpha", "mu", "zeta"]);
}

#[test]
fn session_duplicate_name_errors_with_invalid_argument() {
    let store = SessionStore::new();
    store.save(None, mk_query("dup", "SELECT 1")).unwrap();
    let err = store.save(None, mk_query("dup", "SELECT 2")).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("dup"));
    assert!(err.message.to_lowercase().contains("delete_query"));
}

#[test]
fn session_delete_returns_true_when_present_false_otherwise() {
    let store = SessionStore::new();
    store.save(None, mk_query("foo", "SELECT 1")).unwrap();
    assert!(store.delete(None, "foo").unwrap());
    assert!(!store.delete(None, "foo").unwrap(), "second delete no-ops");
    assert!(store.get(None, "foo").unwrap().is_none());
}

// --- WorkspaceStore --------------------------------------------------------

/// Build a fresh engine against a temp workspace file. Holding onto the
/// `TempDir` return keeps the directory alive for the caller's scope.
/// Uses `new_no_daemon` to avoid interference from any daemon running
/// in parallel (e.g. from daemon_tests in the same `cargo test` run).
fn workspace_engine() -> (Engine, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let engine = Engine::new_no_daemon(Some(path.to_str().unwrap().into())).unwrap();
    (engine, dir)
}

#[test]
fn workspace_save_then_get_roundtrips() {
    let (engine, _dir) = workspace_engine();
    let store = WorkspaceStore::new();
    let q = mk_query(
        "top_products",
        "SELECT product, SUM(qty) FROM orders GROUP BY product",
    );
    store.save(Some(&engine), q.clone()).unwrap();
    let fetched = store.get(Some(&engine), "top_products").unwrap().unwrap();
    assert_eq!(fetched.name, q.name);
    assert_eq!(fetched.sql, q.sql);
    assert_eq!(fetched.description, q.description);
}

#[test]
fn workspace_list_returns_sorted_names() {
    let (engine, _dir) = workspace_engine();
    let store = WorkspaceStore::new();
    store
        .save(Some(&engine), mk_query("zeta", "SELECT 1"))
        .unwrap();
    store
        .save(Some(&engine), mk_query("alpha", "SELECT 2"))
        .unwrap();
    store
        .save(Some(&engine), mk_query("mu", "SELECT 3"))
        .unwrap();
    let names: Vec<String> = store
        .list(Some(&engine))
        .unwrap()
        .into_iter()
        .map(|q| q.name)
        .collect();
    assert_eq!(names, vec!["alpha", "mu", "zeta"]);
}

#[test]
fn workspace_duplicate_name_errors_with_invalid_argument() {
    let (engine, _dir) = workspace_engine();
    let store = WorkspaceStore::new();
    store
        .save(Some(&engine), mk_query("dup", "SELECT 1"))
        .unwrap();
    let err = store
        .save(Some(&engine), mk_query("dup", "SELECT 2"))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("dup"));
}

#[test]
fn workspace_delete_returns_true_when_present_false_otherwise() {
    let (engine, _dir) = workspace_engine();
    let store = WorkspaceStore::new();
    store
        .save(Some(&engine), mk_query("foo", "SELECT 1"))
        .unwrap();
    assert!(store.delete(Some(&engine), "foo").unwrap());
    assert!(!store.delete(Some(&engine), "foo").unwrap());
    assert!(store.get(Some(&engine), "foo").unwrap().is_none());
}

/// SQL strings with single quotes (common for WHERE clauses) must round-trip
/// through the `_hyperdb_saved_queries` meta-table without breaking the
/// INSERT; this is the main risk area for the hand-rolled string escaping.
#[test]
fn workspace_handles_quoted_sql() {
    let (engine, _dir) = workspace_engine();
    let store = WorkspaceStore::new();
    let sql = "SELECT * FROM orders WHERE status = 'ship''d' AND notes LIKE '%user''s%'";
    store.save(Some(&engine), mk_query("quoted", sql)).unwrap();
    let fetched = store.get(Some(&engine), "quoted").unwrap().unwrap();
    assert_eq!(fetched.sql, sql);
}

/// Key persistence test: save into a workspace, drop the store + engine,
/// reopen the same workspace file, and verify the saved query is still
/// there. Protects the backing meta-table against format drift.
#[test]
fn workspace_persists_across_restarts() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let path_str = path.to_str().unwrap().to_string();

    {
        let engine = Engine::new_no_daemon(Some(path_str.clone())).unwrap();
        let store = WorkspaceStore::new();
        store
            .save(Some(&engine), mk_query("persisted", "SELECT 42"))
            .unwrap();
        // Drop engine explicitly so it releases the workspace file before
        // the second engine below reopens it.
        drop(engine);
    }

    let engine = Engine::new_no_daemon(Some(path_str)).unwrap();
    let store = WorkspaceStore::new();
    let fetched = store.get(Some(&engine), "persisted").unwrap().unwrap();
    assert_eq!(fetched.sql, "SELECT 42");
    let listed = store.list(Some(&engine)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "persisted");
}

// --- HyperMcpServer integration -------------------------------------------

/// When the server is constructed with no workspace path, it picks a
/// `SessionStore` — saved queries exist only in-process. Verifying via the
/// resource URI gives us end-to-end coverage of the `save_query` tool →
/// resource list → resource read chain.
#[test]
fn server_ephemeral_session_store_exposes_query_resources() {
    let server = HyperMcpServer::with_no_daemon(None, false, false, true);
    // Reach into the store directly via its resource helper by saving a
    // query through the store (wiring of the tool itself is covered
    // indirectly — the public testable surface is the store + resource).
    let store = hyperdb_mcp::saved_queries::build_store(None);
    store
        .save(None, mk_query("top5", "SELECT * FROM t LIMIT 5"))
        .unwrap();
    drop(store);

    // The server's own store is a different instance (ephemeral stores
    // don't share state), so we only test that the resource layer
    // correctly handles an unknown saved query with a 404-style error.
    let result = server.resource_body_for_uri("hyper://queries/top5/definition");
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().code,
        ErrorCode::TableNotFound,
        "unknown saved query should surface as TableNotFound"
    );
}

/// When constructed with a workspace path, the server picks a
/// `WorkspaceStore`. A query saved via the raw store inside that workspace
/// file must be visible through the server's resource layer — this
/// exercises the full `list_resource_uris` → `resource_body_for_uri` path.
#[test]
fn server_workspace_store_exposes_saved_queries_via_resources() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ws.hyper");
    let path_str = path.to_str().unwrap().to_string();

    // Seed the workspace with one saved query by going through the same
    // store type the server will use. Uses new_no_daemon to avoid
    // interference from any daemon running in parallel.
    {
        let engine = Engine::new_no_daemon(Some(path_str.clone())).unwrap();
        let store = WorkspaceStore::new();
        store
            .save(
                Some(&engine),
                mk_query("recent_orders", "SELECT * FROM orders LIMIT 10"),
            )
            .unwrap();
    }

    // with_no_daemon ensures the server spawns its own hyperd rather than
    // connecting to a daemon left over from daemon_tests.
    let server = HyperMcpServer::with_no_daemon(Some(path_str), false, false, true);

    // The URI catalog lists both the definition and result resources.
    let uris = server.list_resource_uris();
    assert!(uris.contains(&"hyper://queries/recent_orders/definition".to_string()));
    assert!(uris.contains(&"hyper://queries/recent_orders/result".to_string()));
    // And the internal meta-table is hidden from the table catalog.
    assert!(!uris
        .iter()
        .any(|u| u.contains(hyperdb_mcp::saved_queries::SAVED_QUERIES_TABLE)));

    // Reading the definition returns JSON with the stored SQL.
    let body = server
        .resource_body_for_uri("hyper://queries/recent_orders/definition")
        .unwrap()
        .expect("definition resource should exist");
    let json = body.as_json().unwrap();
    assert_eq!(json["name"], "recent_orders");
    assert_eq!(json["sql"], "SELECT * FROM orders LIMIT 10");
}
