// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! End-to-end MCP test harness.
//!
//! These tests spin up a `HyperMcpServer` and a minimal `ClientHandler`
//! on opposite halves of an in-memory `tokio::io::duplex` pair, then
//! invoke tools via the rmcp client API. Coverage here goes through the
//! full rmcp dispatch path — params deserialization, request-context
//! plumbing, error mapping — exercising server-handler behavior that
//! engine-level tests can't reach.

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientInfo};
use rmcp::service::{RoleClient, RunningService};
use rmcp::{ClientHandler, ServiceExt};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

use hyperdb_mcp::server::HyperMcpServer;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Minimal client handler — its only job is to satisfy `ServiceExt`
/// so the server-side tool calls can be issued.
#[derive(Debug, Clone)]
struct DummyClientHandler;

impl ClientHandler for DummyClientHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

/// In-memory client+server pair backed by a `tokio::io::duplex`.
struct TestHarness {
    client: RunningService<RoleClient, DummyClientHandler>,
    server_handle: tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    /// Persistent workspace path — kept alive via the temp dir.
    /// Held by the harness so individual tests can read it back if a
    /// scenario ever needs to inspect the on-disk file directly.
    #[expect(
        dead_code,
        reason = "kept for future tests that inspect the on-disk persistent file"
    )]
    persistent_path: PathBuf,
    _temp_dir: Arc<TempDir>,
}

impl TestHarness {
    /// Spin up a server with a fresh persistent workspace + an
    /// in-memory client. `read_only=false` is the typical case.
    /// `ephemeral_only=true` skips the persistent attachment.
    async fn start(
        read_only: bool,
        ephemeral_only: bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let temp_dir = Arc::new(TempDir::new()?);
        let persistent_path = temp_dir.path().join("workspace.hyper");

        let (server_io, client_io) = tokio::io::duplex(64 * 1024);

        let workspace = if ephemeral_only {
            None
        } else {
            Some(persistent_path.to_string_lossy().to_string())
        };
        let server = HyperMcpServer::with_no_daemon(workspace, read_only, true);

        let server_handle = tokio::spawn(async move {
            let running = server
                .serve(server_io)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
            running
                .waiting()
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
            Ok(())
        });

        let client = DummyClientHandler
            .serve(client_io)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

        Ok(Self {
            client,
            server_handle,
            persistent_path,
            _temp_dir: temp_dir,
        })
    }

    async fn shutdown(self) -> TestResult {
        self.client
            .cancel()
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        self.server_handle.await??;
        Ok(())
    }
}

/// Helper — invoke a tool by name, building the request params from a
/// JSON value's top-level object fields.
async fn call_tool(
    client: &RunningService<RoleClient, DummyClientHandler>,
    name: &'static str,
    args: serde_json::Value,
) -> Result<CallToolResult, Box<dyn std::error::Error + Send + Sync>> {
    let arguments = args.as_object().cloned();
    let params = match arguments {
        Some(args) => CallToolRequestParams::new(name).with_arguments(args),
        None => CallToolRequestParams::new(name),
    };
    let result = client
        .call_tool(params)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
    Ok(result)
}

/// First text-content block from a tool result.
fn first_text(result: &CallToolResult) -> Option<String> {
    result
        .content
        .first()
        .and_then(|c| c.raw.as_text())
        .map(|t| t.text.clone())
}

/// Concatenated text of every content block — `query` returns two
/// (the formatted SQL and the JSON body), so any payload check needs
/// to look at the full set.
fn all_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.raw.as_text())
        .map(|t| t.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Did the tool return an `is_error: true` content block?
fn is_error(result: &CallToolResult) -> bool {
    result.is_error.unwrap_or(false)
}

// =====================================================================
// Four "now works" happy paths — PR #31 rejections lifted by PR #32.
// =====================================================================

/// `load_files(persist=true)` reaches the per-target pool branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_load_files_persist_via_router_now_works() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let temp = TempDir::new()?;
    let csv_path = temp.path().join("rows.csv");
    std::fs::write(&csv_path, b"id,name\n1,alice\n2,bob\n")?;

    let result = call_tool(
        &h.client,
        "load_files",
        serde_json::json!({
            "files": [{
                "path": csv_path.to_string_lossy(),
                "table": "p_rows",
                "format": "csv",
            }],
            "persist": true,
        }),
    )
    .await?;

    assert!(
        !is_error(&result),
        "load_files+persist must succeed; got: {:?}",
        first_text(&result)
    );

    let q = call_tool(
        &h.client,
        "query",
        serde_json::json!({
            "sql": "SELECT COUNT(*) AS n FROM \"persistent\".\"public\".\"p_rows\""
        }),
    )
    .await?;
    let body = all_text(&q);
    assert!(
        body.contains("\"n\":2") || body.contains("\"n\": 2"),
        "got: {body}"
    );

    h.shutdown().await
}

/// `load_file(mode="merge", database="persistent")` accepts non-primary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_load_file_merge_database_now_works() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let temp = TempDir::new()?;

    let csv1 = temp.path().join("seed.csv");
    std::fs::write(&csv1, b"id,name\n1,alice\n2,bob\n")?;
    let r = call_tool(
        &h.client,
        "load_file",
        serde_json::json!({
            "path": csv1.to_string_lossy(),
            "table": "merge_t",
            "format": "csv",
            "mode": "append",
            "database": "persistent",
        }),
    )
    .await?;
    assert!(!is_error(&r), "seed append failed: {:?}", first_text(&r));

    let csv2 = temp.path().join("update.csv");
    std::fs::write(&csv2, b"id,name\n2,robert\n3,carol\n")?;
    let r = call_tool(
        &h.client,
        "load_file",
        serde_json::json!({
            "path": csv2.to_string_lossy(),
            "table": "merge_t",
            "format": "csv",
            "mode": "merge",
            "merge_key": ["id"],
            "database": "persistent",
        }),
    )
    .await?;
    assert!(!is_error(&r), "merge failed: {:?}", first_text(&r));

    let q = call_tool(
        &h.client,
        "query",
        serde_json::json!({
            "sql": "SELECT COUNT(*) AS n FROM \"persistent\".\"public\".\"merge_t\""
        }),
    )
    .await?;
    let body = all_text(&q);
    assert!(
        body.contains("\"n\":3") || body.contains("\"n\": 3"),
        "got: {body}"
    );

    h.shutdown().await
}

/// `export(format="hyper", database="persistent")` snapshots the
/// requested database (was always primary pre-#32).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_export_hyper_database_now_works() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let temp = TempDir::new()?;

    let csv = temp.path().join("src.csv");
    std::fs::write(&csv, b"id,name\n1,alice\n2,bob\n")?;
    let r = call_tool(
        &h.client,
        "load_file",
        serde_json::json!({
            "path": csv.to_string_lossy(),
            "table": "exp_t",
            "format": "csv",
            "mode": "append",
            "database": "persistent",
        }),
    )
    .await?;
    assert!(!is_error(&r), "seed failed: {:?}", first_text(&r));

    let out_path = temp.path().join("out.hyper");
    let r = call_tool(
        &h.client,
        "export",
        serde_json::json!({
            "format": "hyper",
            "path": out_path.to_string_lossy(),
            "database": "persistent",
        }),
    )
    .await?;
    assert!(!is_error(&r), "export failed: {:?}", first_text(&r));
    assert!(out_path.exists(), "export must produce the .hyper file");

    h.shutdown().await
}

/// `watch_directory(persist=true)` builds a per-target pool against
/// the persistent workspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_watch_directory_persist_via_router_now_works() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let watch_dir = TempDir::new()?;

    let r = call_tool(
        &h.client,
        "execute",
        serde_json::json!({
            "sql": "CREATE TABLE \"persistent\".\"public\".\"w_events\" (id INT, name TEXT)"
        }),
    )
    .await?;
    assert!(!is_error(&r), "create table failed: {:?}", first_text(&r));

    let r = call_tool(
        &h.client,
        "watch_directory",
        serde_json::json!({
            "path": watch_dir.path().to_string_lossy(),
            "table": "w_events",
            "persist": true,
        }),
    )
    .await?;
    assert!(
        !is_error(&r),
        "watch_directory failed: {:?}",
        first_text(&r)
    );

    let csv = watch_dir.path().join("batch.csv");
    std::fs::write(&csv, b"id,name\n1,alice\n2,bob\n")?;
    let ready = watch_dir.path().join("batch.csv.ready");
    std::fs::write(&ready, b"")?;

    let canon = watch_dir.path().canonicalize()?;
    let data_path = canon.join("batch.csv");
    let ready_path = canon.join("batch.csv.ready");
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(10) {
        if !data_path.exists() && !ready_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        !data_path.exists(),
        "watcher did not ingest within 10s; .csv still present"
    );

    let _ = call_tool(
        &h.client,
        "unwatch_directory",
        serde_json::json!({ "path": canon.to_string_lossy() }),
    )
    .await?;

    let q = call_tool(
        &h.client,
        "query",
        serde_json::json!({
            "sql": "SELECT COUNT(*) AS n FROM \"persistent\".\"public\".\"w_events\""
        }),
    )
    .await?;
    let body = all_text(&q);
    assert!(
        body.contains("\"n\":2") || body.contains("\"n\": 2"),
        "got: {body}"
    );

    h.shutdown().await
}

// =====================================================================
// PR #31 rejection / routing paths via the rmcp dispatcher.
// =====================================================================

/// `--ephemeral-only` + `persist:true` → `InvalidArgument`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ephemeral_only_plus_persist_returns_invalid_argument() -> TestResult {
    let h = TestHarness::start(false, true).await?;

    let temp = TempDir::new()?;
    let csv = temp.path().join("rows.csv");
    std::fs::write(&csv, b"id\n1\n")?;

    let result = call_tool(
        &h.client,
        "load_files",
        serde_json::json!({
            "files": [{
                "path": csv.to_string_lossy(),
                "table": "t",
                "format": "csv",
            }],
            "persist": true,
        }),
    )
    .await?;

    assert!(is_error(&result), "must reject persist when ephemeral-only");
    let msg = first_text(&result).unwrap_or_default();
    assert!(
        msg.contains("ephemeral-only") || msg.contains("persistent"),
        "error must mention the cause; got: {msg}"
    );

    h.shutdown().await
}

/// `database="Persistent"` is accepted case-insensitively.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn database_persistent_case_insensitive_routes_correctly() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let temp = TempDir::new()?;
    let csv = temp.path().join("rows.csv");
    std::fs::write(&csv, b"id\n1\n2\n")?;

    let r = call_tool(
        &h.client,
        "load_file",
        serde_json::json!({
            "path": csv.to_string_lossy(),
            "table": "case_t",
            "format": "csv",
            "mode": "append",
            "database": "Persistent",
        }),
    )
    .await?;
    assert!(
        !is_error(&r),
        "case-insensitive Persistent must route to persistent: {:?}",
        first_text(&r)
    );

    let q = call_tool(
        &h.client,
        "query",
        serde_json::json!({
            "sql": "SELECT COUNT(*) AS n FROM \"persistent\".\"public\".\"case_t\""
        }),
    )
    .await?;
    let body = all_text(&q);
    assert!(
        body.contains("\"n\":2") || body.contains("\"n\": 2"),
        "got: {body}"
    );

    h.shutdown().await
}

/// Both `database` and `persist` set: `database` wins. Test routes
/// "Local" + `persist:true` to primary, not persistent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_true_plus_database_local_lets_database_win() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let temp = TempDir::new()?;
    let csv = temp.path().join("rows.csv");
    std::fs::write(&csv, b"id\n1\n2\n3\n")?;

    let r = call_tool(
        &h.client,
        "load_file",
        serde_json::json!({
            "path": csv.to_string_lossy(),
            "table": "wins_t",
            "format": "csv",
            "mode": "append",
            "database": "Local",
            "persist": true,
        }),
    )
    .await?;
    assert!(!is_error(&r), "load_file failed: {:?}", first_text(&r));

    let q_primary = call_tool(
        &h.client,
        "query",
        serde_json::json!({ "sql": "SELECT COUNT(*) AS n FROM wins_t" }),
    )
    .await?;
    let primary = all_text(&q_primary);
    assert!(
        primary.contains("\"n\":3") || primary.contains("\"n\": 3"),
        "rows must land in primary; got: {primary}"
    );

    let q_persistent = call_tool(
        &h.client,
        "query",
        serde_json::json!({
            "sql": "SELECT COUNT(*) AS n FROM \"persistent\".\"public\".\"wins_t\""
        }),
    )
    .await?;
    let persistent_text = all_text(&q_persistent);
    let zero_or_err = is_error(&q_persistent)
        || persistent_text.contains("\"n\":0")
        || persistent_text.contains("\"n\": 0");
    assert!(
        zero_or_err,
        "rows must NOT be in persistent; got: {persistent_text}"
    );

    h.shutdown().await
}

// =====================================================================
// Iter 4-5 paths via the rmcp dispatcher.
// =====================================================================

/// `set_table_metadata(database="persistent", ...)` updates the per-DB
/// catalog row in the persistent attachment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_set_table_metadata_database_persistent() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let temp = TempDir::new()?;

    let csv = temp.path().join("rows.csv");
    std::fs::write(&csv, b"id,name\n1,alice\n")?;
    let r = call_tool(
        &h.client,
        "load_file",
        serde_json::json!({
            "path": csv.to_string_lossy(),
            "table": "meta_t",
            "format": "csv",
            "mode": "append",
            "database": "persistent",
        }),
    )
    .await?;
    assert!(!is_error(&r), "seed failed: {:?}", first_text(&r));

    let r = call_tool(
        &h.client,
        "set_table_metadata",
        serde_json::json!({
            "table": "meta_t",
            "database": "persistent",
            "purpose": "test fixture",
            "license": "CC0",
        }),
    )
    .await?;
    assert!(
        !is_error(&r),
        "set_table_metadata failed: {:?}",
        first_text(&r)
    );

    let q = call_tool(
        &h.client,
        "query",
        serde_json::json!({
            "sql": "SELECT purpose, license FROM \"persistent\".\"public\".\"_table_catalog\" \
                    WHERE table_name = 'meta_t'"
        }),
    )
    .await?;
    let body = all_text(&q);
    assert!(
        body.contains("test fixture"),
        "purpose missing; got: {body}"
    );
    assert!(body.contains("CC0"), "license missing; got: {body}");

    h.shutdown().await
}

/// `detach_database` while a watcher is active rejects with
/// `InvalidArgument`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_detach_database_rejects_when_watcher_active() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let attach_dir = TempDir::new()?;
    let watch_dir = TempDir::new()?;
    let attached_file = attach_dir.path().join("attached.hyper");

    let r = call_tool(
        &h.client,
        "attach_database",
        serde_json::json!({
            "alias": "user_db",
            "kind": "local_file",
            "path": attached_file.to_string_lossy(),
            "writable": true,
            "on_missing": "create",
        }),
    )
    .await?;
    assert!(!is_error(&r), "attach failed: {:?}", first_text(&r));

    let r = call_tool(
        &h.client,
        "execute",
        serde_json::json!({
            "sql": "CREATE TABLE \"user_db\".\"public\".\"events\" (id INT, name TEXT)"
        }),
    )
    .await?;
    assert!(!is_error(&r), "create table failed: {:?}", first_text(&r));

    let r = call_tool(
        &h.client,
        "watch_directory",
        serde_json::json!({
            "path": watch_dir.path().to_string_lossy(),
            "table": "events",
            "database": "user_db",
        }),
    )
    .await?;
    assert!(!is_error(&r), "watch failed: {:?}", first_text(&r));

    let r = call_tool(
        &h.client,
        "detach_database",
        serde_json::json!({ "alias": "user_db" }),
    )
    .await?;
    assert!(
        is_error(&r),
        "detach must be rejected while watcher is active"
    );
    let msg = first_text(&r).unwrap_or_default();
    assert!(
        msg.contains("watcher") || msg.contains("unwatch_directory"),
        "error must guide the user; got: {msg}"
    );

    let canon = watch_dir.path().canonicalize()?;
    let _ = call_tool(
        &h.client,
        "unwatch_directory",
        serde_json::json!({ "path": canon.to_string_lossy() }),
    )
    .await?;

    h.shutdown().await
}

/// Regression test for the final-sweep CRITICAL: `copy_query` did not
/// canonicalize `target_database`, so attaching as `"My_DB"` (which
/// the registry stores lowercased as `"my_db"`) and calling
/// `copy_query(target_database="My_DB")` failed at SQL render time
/// because Hyper is case-sensitive on quoted identifiers. The fix
/// lowercases `target_database` after the LOCAL_ALIAS filter so both
/// the registry lookup and `qualified_name` agree on the canonical
/// form.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_copy_query_target_database_mixed_case_canonicalizes() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let attach_dir = TempDir::new()?;
    let attached_file = attach_dir.path().join("dst.hyper");

    let r = call_tool(
        &h.client,
        "attach_database",
        serde_json::json!({
            "alias": "My_DB",
            "kind": "local_file",
            "path": attached_file.to_string_lossy(),
            "writable": true,
            "on_missing": "create",
        }),
    )
    .await?;
    assert!(!is_error(&r), "attach failed: {:?}", first_text(&r));

    // Use the user-typed mixed-case alias for the copy target. Pre-fix
    // this would render `"My_DB"."public"."t"` and fail; post-fix the
    // tool lowercases to match the canonical `"my_db"` form.
    let r = call_tool(
        &h.client,
        "copy_query",
        serde_json::json!({
            "mode": "create",
            "target_database": "My_DB",
            "target_table": "t",
            "sql": "SELECT 1 AS x, 'hi' AS y",
        }),
    )
    .await?;
    assert!(
        !is_error(&r),
        "copy_query with mixed-case target_database must succeed; got: {:?}",
        first_text(&r)
    );

    let q = call_tool(
        &h.client,
        "query",
        serde_json::json!({
            "sql": "SELECT COUNT(*) AS n FROM \"my_db\".\"public\".\"t\""
        }),
    )
    .await?;
    let body = all_text(&q);
    assert!(
        body.contains("\"n\":1") || body.contains("\"n\": 1"),
        "row must land in canonical lowercase database; got: {body}"
    );

    h.shutdown().await
}
