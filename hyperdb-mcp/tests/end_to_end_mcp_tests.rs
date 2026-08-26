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
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

use hyperdb_mcp::engine::Engine;
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
    /// Shared engine handle retained for status-lock regression scenarios.
    engine_handle: Arc<Mutex<Option<Engine>>>,
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
        let engine_handle = server.engine_handle();

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
            engine_handle,
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

/// A native thread holds the engine mutex until this guard is dropped. Keeping
/// the lock outside Tokio means a non-Send `MutexGuard` never crosses an
/// `.await`, while `Drop` releases the fixture even when an assertion panics.
struct EngineLockHolder {
    release: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EngineLockHolder {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("engine-lock fixture thread must finish");
        }
    }
}

/// Lock the server engine on a native thread and wait until it definitely owns
/// the mutex before issuing a status request.
fn hold_engine_lock(engine_handle: Arc<Mutex<Option<Engine>>>) -> EngineLockHolder {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _guard = engine_handle.lock().expect("engine mutex");
        ready_tx.send(()).expect("test must await engine lock");
        release_rx.recv().expect("test must release engine lock");
    });

    ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("engine-lock fixture must become ready promptly");

    EngineLockHolder {
        release: Some(release_tx),
        thread: Some(thread),
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

/// Parse the sole JSON text block emitted by the `status` tool.
fn status_json(result: &CallToolResult) -> serde_json::Value {
    serde_json::from_str(&first_text(result).expect("status must return a text payload"))
        .expect("status payload must be JSON")
}

/// A normal response and the lock-contended fallback have one installation
/// identity contract. Only engine-dependent statistics may be absent when the
/// fallback says `engine_busy: true`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_full_and_degraded_share_identity_contract() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    // Ensure the server's eager warm-up has completed before observing the
    // uncontended branch; otherwise startup itself legitimately uses the
    // degraded response while the engine is still absent.
    let warm_up = call_tool(
        &h.client,
        "query",
        serde_json::json!({ "sql": "SELECT 1 AS ready" }),
    )
    .await?;
    assert!(
        !is_error(&warm_up),
        "query must initialize the engine: {:?}",
        first_text(&warm_up)
    );

    let full_result = call_tool(&h.client, "status", serde_json::json!({})).await?;
    assert!(!is_error(&full_result), "full status must succeed");
    let full = status_json(&full_result);
    assert_eq!(full["engine_busy"], false, "uncontended status is full");

    let _engine_lock = hold_engine_lock(Arc::clone(&h.engine_handle));
    let degraded_result = call_tool(&h.client, "status", serde_json::json!({})).await?;
    assert!(!is_error(&degraded_result), "degraded status must succeed");
    let degraded = status_json(&degraded_result);
    assert_eq!(degraded["engine_busy"], true, "lock contention is explicit");

    for key in [
        "mcp_version",
        "hyper_rust_api_version",
        "installation",
        "default_database",
    ] {
        assert!(
            full.get(key).is_some(),
            "full status missing `{key}`: {full}"
        );
        assert!(
            degraded.get(key).is_some(),
            "degraded status missing `{key}`: {degraded}"
        );
        assert_eq!(
            full[key], degraded[key],
            "full and degraded status disagree on `{key}`"
        );
    }

    assert_eq!(
        full["mcp_version"],
        hyperdb_mcp::version::mcp_version_string()
    );
    assert_eq!(
        full["hyper_rust_api_version"],
        hyperdb_mcp::version::hyper_api_version_string()
    );
    assert!(
        full["installation"].is_object(),
        "installation must be a structured identity: {}",
        full["installation"]
    );
    assert_eq!(full["default_database"], "local");

    drop(_engine_lock);
    h.shutdown().await
}

/// The status fast path must not wait behind an in-flight data-plane lock and
/// must honestly omit fields that require that lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_degraded_returns_promptly_while_engine_locked() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let _engine_lock = hold_engine_lock(Arc::clone(&h.engine_handle));

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        call_tool(&h.client, "status", serde_json::json!({})),
    )
    .await
    .expect("status must return before the explicit one-second bound")?;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "status exceeded its one-second prompt-return bound"
    );
    assert!(!is_error(&result), "degraded status must succeed");
    let status = status_json(&result);

    assert_eq!(status["engine_busy"], true);
    assert_eq!(
        status["mcp_version"],
        hyperdb_mcp::version::mcp_version_string(),
        "degraded status must retain the MCP identity"
    );
    assert_eq!(
        status["hyper_rust_api_version"],
        hyperdb_mcp::version::hyper_api_version_string(),
        "degraded status must retain the underlying API identity"
    );
    assert!(
        status["installation"].is_object(),
        "degraded status must retain installation identity"
    );
    assert_eq!(status["default_database"], "local");
    for omitted in [
        "table_count",
        "total_rows",
        "disk_usage_bytes",
        "ephemeral_path",
        "logs",
    ] {
        assert!(
            status.get(omitted).is_none(),
            "degraded status must omit `{omitted}`: {status}"
        );
    }

    drop(_engine_lock);
    h.shutdown().await
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
            "sql": ["CREATE TABLE \"persistent\".\"public\".\"w_events\" (id INT, name TEXT)"]
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
            "sql": ["CREATE TABLE \"user_db\".\"public\".\"events\" (id INT, name TEXT)"]
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

// =====================================================================
// Atomic multi-statement execute — exercises the full MCP handler
// path including validation, the transaction wrapper, the search-path
// guard, and the response envelope. The engine-level primitive is
// already covered in transaction_tests.rs; these tests guard the glue.
// =====================================================================

/// Multi-statement upsert via the MCP `execute` tool: both statements
/// commit together, the response carries `per_statement` entries and a
/// summed `affected_rows`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_execute_multi_statement_upsert_commits_atomically() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    let r = call_tool(
        &h.client,
        "execute",
        serde_json::json!({
            "sql": ["CREATE TABLE settings (key TEXT NOT NULL, value TEXT NOT NULL)"]
        }),
    )
    .await?;
    assert!(!is_error(&r), "create table failed: {:?}", first_text(&r));

    // Atomic upsert. INSERT lands because the row is missing.
    let r = call_tool(
        &h.client,
        "execute",
        serde_json::json!({
            "sql": [
                "UPDATE settings SET value = 'dark' WHERE key = 'theme'",
                "INSERT INTO settings (key, value) SELECT 'theme', 'dark' \
                   WHERE NOT EXISTS (SELECT 1 FROM settings WHERE key = 'theme')"
            ]
        }),
    )
    .await?;
    assert!(!is_error(&r), "upsert failed: {:?}", first_text(&r));
    let body = all_text(&r);
    assert!(
        body.contains("\"statements\": 2") || body.contains("\"statements\":2"),
        "response must report 2 statements; got: {body}"
    );
    assert!(
        body.contains("\"operation\": \"transaction\"")
            || body.contains("\"operation\":\"transaction\""),
        "response stats must report a transaction; got: {body}"
    );

    let q = call_tool(
        &h.client,
        "query",
        serde_json::json!({
            "sql": "SELECT value FROM settings WHERE key = 'theme'"
        }),
    )
    .await?;
    let body = all_text(&q);
    assert!(body.contains("dark"), "row not committed: {body}");

    h.shutdown().await
}

/// Mid-batch failure rolls back through the MCP handler. The error
/// response names the failing statement index, and the table is
/// observable as empty after the error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_execute_multi_statement_rolls_back_on_failure() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    let r = call_tool(
        &h.client,
        "execute",
        serde_json::json!({
            "sql": ["CREATE TABLE t (id INT NOT NULL)"]
        }),
    )
    .await?;
    assert!(!is_error(&r), "create table failed: {:?}", first_text(&r));

    // The second INSERT violates NOT NULL — entire batch must roll back.
    let r = call_tool(
        &h.client,
        "execute",
        serde_json::json!({
            "sql": [
                "INSERT INTO t (id) VALUES (1)",
                "INSERT INTO t (id) VALUES (NULL)"
            ]
        }),
    )
    .await?;
    assert!(is_error(&r), "expected rollback error, got success");
    let body = all_text(&r);
    assert!(
        body.contains("statement 2 of 2 failed"),
        "error must name failing index; got: {body}"
    );

    let q = call_tool(
        &h.client,
        "query",
        serde_json::json!({"sql": "SELECT COUNT(*) AS n FROM t"}),
    )
    .await?;
    let body = all_text(&q);
    assert!(
        body.contains("\"n\":0") || body.contains("\"n\": 0"),
        "first INSERT must have rolled back; got: {body}"
    );

    h.shutdown().await
}

/// Validation rejects DDL+DML mixing before any SQL is sent to hyperd.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_execute_rejects_ddl_dml_mix() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let r = call_tool(
        &h.client,
        "execute",
        serde_json::json!({
            "sql": ["CREATE TABLE x (i INT)", "INSERT INTO x VALUES (1)"]
        }),
    )
    .await?;
    assert!(is_error(&r), "DDL+DML mix must be rejected up front");
    let body = all_text(&r);
    assert!(
        body.to_lowercase().contains("ddl") && body.to_lowercase().contains("dml"),
        "error must explain the rule; got: {body}"
    );
    h.shutdown().await
}

/// Single-element batch keeps auto-commit behavior; response reports
/// `operation: "command"` so callers can still distinguish singletons
/// from transactions when needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_execute_singleton_uses_auto_commit_path() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let r = call_tool(
        &h.client,
        "execute",
        serde_json::json!({"sql": ["CREATE TABLE t (i INT)"]}),
    )
    .await?;
    assert!(!is_error(&r), "create failed: {:?}", first_text(&r));
    let body = all_text(&r);
    assert!(
        body.contains("\"operation\": \"command\"") || body.contains("\"operation\":\"command\""),
        "singleton must report operation=command; got: {body}"
    );
    assert!(
        body.contains("\"statements\": 1") || body.contains("\"statements\":1"),
        "singleton must report statements=1; got: {body}"
    );
    h.shutdown().await
}
