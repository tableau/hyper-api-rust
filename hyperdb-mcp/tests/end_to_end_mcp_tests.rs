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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::server::HyperMcpServer;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

const PERSISTENT_LOCK_MCP_CHILD_ENV: &str = "HYPERDB_MCP_PERSISTENT_LOCK_MCP_CHILD";

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

    /// Same in-memory MCP harness, but attaches a caller-owned persistent
    /// workspace. The parent of the self-child fixture owns that path's RAII
    /// directory, so it remains valid for the complete contention scenario.
    async fn start_at_persistent(
        persistent_path: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let temp_dir = Arc::new(TempDir::new()?);
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let workspace = Some(persistent_path.to_string_lossy().to_string());
        let server = HyperMcpServer::with_no_daemon(workspace, false, true);
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

/// Record a successful object response that must remain mirrored between its
/// sole text block and `structuredContent`. Returns the parsed text payload so
/// callers can pin their tool-specific legacy fields as well.
fn record_object_response(
    failures: &mut Vec<String>,
    case: &str,
    result: &CallToolResult,
    expected_database: &str,
    expected_fields: &[&str],
) -> Option<serde_json::Value> {
    if is_error(result) {
        failures.push(format!(
            "{case}: tool returned an error: {:?}",
            first_text(result)
        ));
    }
    if result.content.len() != 1 {
        failures.push(format!(
            "{case}: expected one JSON text block, got {} content blocks",
            result.content.len()
        ));
    }

    let Some(text) = result
        .content
        .first()
        .and_then(|content| content.raw.as_text())
        .map(|content| content.text.as_str())
    else {
        failures.push(format!("{case}: first content block must be text JSON"));
        return None;
    };
    let payload = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(payload) => payload,
        Err(error) => {
            failures.push(format!("{case}: text block is not JSON: {error}"));
            return None;
        }
    };

    if result.structured_content.as_ref() != Some(&payload) {
        failures.push(format!(
            "{case}: structuredContent must exactly mirror the JSON text block"
        ));
    }
    let Some(object) = payload.as_object() else {
        failures.push(format!("{case}: JSON payload must be an object"));
        return Some(payload);
    };
    let mut actual_fields: Vec<_> = object.keys().map(String::as_str).collect();
    actual_fields.sort_unstable();
    let mut expected_fields = expected_fields.to_vec();
    expected_fields.sort_unstable();
    if actual_fields != expected_fields {
        failures.push(format!(
            "{case}: top-level fields changed: expected {expected_fields:?}, got {actual_fields:?}"
        ));
    }
    if object.get("resolved_database") != Some(&serde_json::json!(expected_database)) {
        failures.push(format!(
            "{case}: resolved_database must be {expected_database:?}, got {:?}",
            object.get("resolved_database")
        ));
    }
    Some(payload)
}

/// Record the query tool's intentionally non-standard two-text-block result.
/// Query has no structuredContent, and its JSON payload is specifically the
/// second block after formatted SQL.
fn record_query_response(
    failures: &mut Vec<String>,
    case: &str,
    result: &CallToolResult,
    expected_database: &str,
    expected_sql: &str,
    expected_rows: serde_json::Value,
) {
    if is_error(result) {
        failures.push(format!(
            "{case}: tool returned an error: {:?}",
            first_text(result)
        ));
    }
    if result.structured_content.is_some() {
        failures.push(format!("{case}: query must not add structuredContent"));
    }
    if result.content.len() != 2 {
        failures.push(format!(
            "{case}: query must preserve SQL-text then JSON-text content order; got {} blocks",
            result.content.len()
        ));
    }

    let sql_text = result
        .content
        .first()
        .and_then(|content| content.raw.as_text())
        .map(|content| content.text.as_str());
    let expected_sql_block = format!("```sql\n{expected_sql}\n```");
    if sql_text != Some(expected_sql_block.as_str()) {
        failures.push(format!(
            "{case}: formatted SQL block changed: expected {expected_sql_block:?}, got {sql_text:?}"
        ));
    }

    let Some(json_text) = result
        .content
        .get(1)
        .and_then(|content| content.raw.as_text())
        .map(|content| content.text.as_str())
    else {
        failures.push(format!("{case}: second query block must be JSON text"));
        return;
    };
    let payload = match serde_json::from_str::<serde_json::Value>(json_text) {
        Ok(payload) => payload,
        Err(error) => {
            failures.push(format!("{case}: second query block is not JSON: {error}"));
            return;
        }
    };
    let Some(object) = payload.as_object() else {
        failures.push(format!("{case}: query JSON payload must be an object"));
        return;
    };
    let mut actual_fields: Vec<_> = object.keys().map(String::as_str).collect();
    actual_fields.sort_unstable();
    let mut expected_fields = vec!["result", "resolved_database", "stats"];
    expected_fields.sort_unstable();
    if actual_fields != expected_fields {
        failures.push(format!(
            "{case}: query top-level fields changed: expected {expected_fields:?}, got {actual_fields:?}"
        ));
    }
    if object.get("result") != Some(&expected_rows) {
        failures.push(format!(
            "{case}: query result changed: expected {expected_rows}, got {:?}",
            object.get("result")
        ));
    }
    if object.get("resolved_database") != Some(&serde_json::json!(expected_database)) {
        failures.push(format!(
            "{case}: resolved_database must be {expected_database:?}, got {:?}",
            object.get("resolved_database")
        ));
    }
    let stats = object.get("stats").and_then(serde_json::Value::as_object);
    let expected_stats = [
        "elapsed_ms",
        "operation",
        "result_size_bytes",
        "rows_returned",
        "rows_scanned",
        "scan_rate_rows_sec",
        "tables_touched",
    ];
    let mut actual_stats = stats
        .map(|stats| stats.keys().map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    actual_stats.sort_unstable();
    if actual_stats != expected_stats {
        failures.push(format!(
            "{case}: query stats fields changed: expected {expected_stats:?}, got {actual_stats:?}"
        ));
    }
    if object["stats"]["operation"] != serde_json::json!("query") {
        failures.push(format!("{case}: query stats.operation must remain query"));
    }
    if object["stats"]["rows_returned"]
        != serde_json::json!(expected_rows.as_array().map_or(0, Vec::len))
    {
        failures.push(format!(
            "{case}: query stats.rows_returned must mirror result length"
        ));
    }
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

/// Persistent contention must be reported by the first persistent-routed
/// operation without making the MCP status endpoint unavailable. The whole
/// potentially blocking scenario runs in an exact self-child owned by the
/// parent, which kills and waits on every timeout/error path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_lock_keeps_mcp_available() -> TestResult {
    if let Some(workspace) = std::env::var_os(PERSISTENT_LOCK_MCP_CHILD_ENV) {
        return run_persistent_lock_mcp_child(PathBuf::from(workspace)).await;
    }

    let temp_dir = TempDir::new()?;
    let workspace = temp_dir.path().join("contended-mcp-persistent.hyper");
    run_contained_mcp_child(
        "persistent_lock_keeps_mcp_available",
        PERSISTENT_LOCK_MCP_CHILD_ENV,
        &workspace,
    );
    Ok(())
}

async fn run_persistent_lock_mcp_child(workspace: PathBuf) -> TestResult {
    let effective_path = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());
    let owner = Engine::new_no_daemon(Some(workspace.to_string_lossy().into_owned()))?;
    let h = TestHarness::start_at_persistent(workspace).await?;

    let status = tokio::time::timeout(
        Duration::from_secs(2),
        call_tool(&h.client, "status", serde_json::json!({})),
    )
    .await
    .expect("status must remain promptly available while persistent attachment is contended")?;
    assert!(
        !is_error(&status),
        "status must stay available despite persistent contention: {}",
        all_text(&status)
    );

    let query = tokio::time::timeout(
        Duration::from_secs(2),
        call_tool(
            &h.client,
            "query",
            serde_json::json!({ "sql": "SELECT 1", "database": "persistent" }),
        ),
    )
    .await
    .expect("persistent-routed query must return before the child bound")?;
    assert!(is_error(&query), "contended persistent query must fail");
    let diagnostic = all_text(&query);
    assert!(
        diagnostic.contains("RESOURCE_BUSY"),
        "must return structured RESOURCE_BUSY: {diagnostic}"
    );
    assert!(
        diagnostic.contains("55006"),
        "must retain SQLSTATE evidence: {diagnostic}"
    );
    assert!(
        diagnostic.contains(effective_path.to_str().unwrap()),
        "must name exact effective persistent path {}: {diagnostic}",
        effective_path.display()
    );
    let lower = diagnostic.to_lowercase();
    assert!(
        lower.contains("doctor"),
        "must include doctor guidance: {diagnostic}"
    );
    assert!(
        lower.contains("possible") && (lower.contains("owner") || lower.contains("process")),
        "must describe a possible owner without accusation: {diagnostic}"
    );

    h.shutdown().await?;
    drop(owner);
    Ok(())
}

fn run_contained_mcp_child(test_name: &str, child_env: &str, workspace: &Path) {
    let mut child =
        Command::new(std::env::current_exe().expect("integration test executable path"))
            .args(["--exact", test_name, "--nocapture"])
            .env(child_env, workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("parent must spawn exact MCP lock helper child");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .expect("parent must collect completed MCP helper output");
                assert!(
                    status.success(),
                    "MCP lock helper failed with {status}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
                return;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let kill_error = child.kill().err();
                let output = child
                    .wait_with_output()
                    .expect("parent must wait for timed-out MCP helper child");
                panic!(
                    "MCP lock helper exceeded its 15s bound and was killed ({kill_error:?})\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
            Err(error) => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .expect("parent must wait after MCP helper status error");
                panic!(
                    "MCP lock helper status check failed: {error}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
        }
    }
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

/// All query-oriented tools expose the canonical database they actually used,
/// without changing their established payloads or MCP content layouts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolved_database_query_success_shapes() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let attached_dir = TempDir::new()?;
    let attached_path = attached_dir.path().join("attached.hyper");
    let mut failures = Vec::new();

    // The primary starts with no user tables (the durable catalog lives in
    // the separate persistent attachment), so this is the successful empty
    // listing branch before the remaining fixture tables are created.
    match call_tool(&h.client, "describe", serde_json::json!({})).await {
        Ok(result) => {
            if let Some(payload) = record_object_response(
                &mut failures,
                "describe empty local listing",
                &result,
                "local",
                &["resolved_database", "tables"],
            ) {
                if payload["tables"] != serde_json::json!([]) {
                    failures.push(format!(
                        "describe empty local listing: tables must remain an empty array, got {:?}",
                        payload.get("tables")
                    ));
                }
            }
        }
        Err(error) => failures.push(format!(
            "describe empty local listing: MCP call failed: {error}"
        )),
    }

    // Fixture setup intentionally uses the normal tool surface so each routed
    // call below observes the same search-path and attachment behavior users
    // get. The calls under test are aggregated later, rather than stopping at
    // the first missing resolved_database field.
    for (case, tool, args) in [
        (
            "setup local table",
            "execute",
            serde_json::json!({ "sql": ["CREATE TABLE local_rows (x INT, label TEXT)"] }),
        ),
        (
            "setup persistent empty table",
            "execute",
            serde_json::json!({
                "sql": ["CREATE TABLE persistent_empty (x INT)"],
                "database": "PERSISTENT"
            }),
        ),
        (
            "setup truncation table",
            "execute",
            serde_json::json!({
                "sql": [
                    "CREATE TABLE truncation_rows AS SELECT i FROM generate_series(1, 10001) s(i)"
                ]
            }),
        ),
        (
            "setup mixed-case attachment",
            "attach_database",
            serde_json::json!({
                "alias": "MiXeD_Attached",
                "kind": "local_file",
                "path": attached_path.to_string_lossy(),
                "writable": true,
                "on_missing": "create"
            }),
        ),
        (
            "setup attached table",
            "execute",
            serde_json::json!({
                "sql": ["CREATE TABLE attached_rows (x INT)"],
                "database": "MIXED_ATTACHED"
            }),
        ),
    ] {
        let result = call_tool(&h.client, tool, args).await?;
        if is_error(&result) {
            return Err(format!("{case} failed: {:?}", first_text(&result)).into());
        }
    }

    macro_rules! call_case {
        ($case:expr, $tool:expr, $args:expr) => {
            match call_tool(&h.client, $tool, $args).await {
                Ok(result) => Some(result),
                Err(error) => {
                    failures.push(format!("{}: MCP call failed: {error}", $case));
                    None
                }
            }
        };
    }

    // Execute preserves both singleton and transaction response shapes while
    // covering the default primary and the canonicalized attached alias.
    if let Some(result) = call_case!(
        "execute local transaction",
        "execute",
        serde_json::json!({
            "sql": [
                "INSERT INTO local_rows VALUES (1, 'local')",
                "INSERT INTO local_rows VALUES (2, 'second')"
            ]
        })
    ) {
        if let Some(payload) = record_object_response(
            &mut failures,
            "execute local transaction",
            &result,
            "local",
            &[
                "affected_rows",
                "per_statement",
                "resolved_database",
                "statements",
                "stats",
            ],
        ) {
            if payload["statements"] != serde_json::json!(2)
                || payload["affected_rows"] != serde_json::json!(2)
                || payload["stats"]["operation"] != serde_json::json!("transaction")
                || payload["per_statement"].as_array().map_or(0, Vec::len) != 2
            {
                failures.push(
                    "execute local transaction: legacy transaction counters or operation changed"
                        .into(),
                );
            }
        }
    }
    if let Some(result) = call_case!(
        "execute attached command",
        "execute",
        serde_json::json!({
            "sql": ["INSERT INTO attached_rows VALUES (7)"],
            "database": "MiXeD_AtTaChEd"
        })
    ) {
        if let Some(payload) = record_object_response(
            &mut failures,
            "execute attached command",
            &result,
            "mixed_attached",
            &[
                "affected_rows",
                "per_statement",
                "resolved_database",
                "statements",
                "stats",
            ],
        ) {
            if payload["statements"] != serde_json::json!(1)
                || payload["affected_rows"] != serde_json::json!(1)
                || payload["stats"]["operation"] != serde_json::json!("command")
                || payload["per_statement"].as_array().map_or(0, Vec::len) != 1
            {
                failures.push(
                    "execute attached command: legacy command counters or operation changed".into(),
                );
            }
        }
    }

    // Query's JSON is deliberately the *second* text block. Exercise both a
    // normal result and the existing successful zero-row result.
    if let Some(result) = call_case!(
        "query local rows",
        "query",
        serde_json::json!({ "sql": "SELECT x FROM local_rows WHERE x = 1" })
    ) {
        record_query_response(
            &mut failures,
            "query local rows",
            &result,
            "local",
            "SELECT\n  x\nFROM\n  local_rows\nWHERE\n  x = 1",
            serde_json::json!([{ "x": 1 }]),
        );
    }
    if let Some(result) = call_case!(
        "query persistent zero rows",
        "query",
        serde_json::json!({
            "sql": "SELECT x FROM persistent_empty",
            "database": "PERSISTENT"
        })
    ) {
        record_query_response(
            &mut failures,
            "query persistent zero rows",
            &result,
            "persistent",
            "SELECT\n  x\nFROM\n  persistent_empty",
            serde_json::json!([]),
        );
    }
    if let Some(result) = call_case!(
        "query attached mixed case",
        "query",
        serde_json::json!({
            "sql": "SELECT x FROM attached_rows",
            "database": "MiXeD_AtTaChEd"
        })
    ) {
        record_query_response(
            &mut failures,
            "query attached mixed case",
            &result,
            "mixed_attached",
            "SELECT\n  x\nFROM\n  attached_rows",
            serde_json::json!([{ "x": 7 }]),
        );
    }
    if let Some(result) = call_case!(
        "query local truncation",
        "query",
        serde_json::json!({ "sql": "SELECT i FROM truncation_rows" })
    ) {
        if is_error(&result) {
            failures.push(format!(
                "query local truncation: tool returned an error: {:?}",
                first_text(&result)
            ));
        }
        if result.structured_content.is_some() {
            failures.push("query local truncation: query must not add structuredContent".into());
        }
        if result.content.len() != 2
            || result
                .content
                .first()
                .and_then(|content| content.raw.as_text())
                .map(|content| content.text.as_str())
                != Some("```sql\nSELECT\n  i\nFROM\n  truncation_rows\n```")
            || result
                .content
                .get(1)
                .and_then(|content| content.raw.as_text())
                .is_none()
        {
            failures.push(
                "query local truncation: content must remain formatted SQL then JSON text".into(),
            );
        }
        let payload = result
            .content
            .get(1)
            .and_then(|content| content.raw.as_text())
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content.text).ok());
        let expected_fields = [
            "hint",
            "resolved_database",
            "result",
            "rows_returned",
            "stats",
            "total_rows",
            "truncated",
        ];
        let mut fields = payload
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .map(|object| object.keys().map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        fields.sort_unstable();
        let rows = payload
            .as_ref()
            .and_then(|payload| payload.get("result"))
            .and_then(serde_json::Value::as_array);
        let expected_hint = "Result set has 10001 rows; only the first 10000 are shown. Add a LIMIT clause, aggregate with GROUP BY, or use the `export` tool to write the full result to a file.";
        if fields != expected_fields
            || rows.map_or(0, Vec::len) != 10_000
            || rows
                .and_then(|rows| rows.first())
                .and_then(|row| row.get("i"))
                != Some(&serde_json::json!(1))
            || rows
                .and_then(|rows| rows.last())
                .and_then(|row| row.get("i"))
                != Some(&serde_json::json!(10_000))
            || payload
                .as_ref()
                .and_then(|payload| payload.get("truncated"))
                != Some(&serde_json::json!(true))
            || payload
                .as_ref()
                .and_then(|payload| payload.get("total_rows"))
                != Some(&serde_json::json!(10_001))
            || payload
                .as_ref()
                .and_then(|payload| payload.get("rows_returned"))
                != Some(&serde_json::json!(10_000))
            || payload.as_ref().and_then(|payload| payload.get("hint"))
                != Some(&serde_json::json!(expected_hint))
            || payload
                .as_ref()
                .and_then(|payload| payload.get("resolved_database"))
                != Some(&serde_json::json!("local"))
            || payload
                .as_ref()
                .and_then(|payload| payload.get("stats"))
                .and_then(|stats| stats.get("rows_returned"))
                != Some(&serde_json::json!(10_000))
        {
            failures.push(format!(
                "query local truncation: legacy truncation fields or resolved database changed: {payload:?}"
            ));
        }
    }

    // Sample has normal and empty successful payloads, each retaining the
    // single text/structured mirror that older clients consume.
    for (case, args, database, table, row_count, sample_size) in [
        (
            "sample local rows",
            serde_json::json!({ "table": "local_rows", "n": 5 }),
            "local",
            "local_rows",
            2,
            2,
        ),
        (
            "sample persistent empty",
            serde_json::json!({
                "table": "persistent_empty",
                "n": 5,
                "database": "PERSISTENT"
            }),
            "persistent",
            "persistent_empty",
            0,
            0,
        ),
        (
            "sample attached mixed case",
            serde_json::json!({
                "table": "attached_rows",
                "n": 5,
                "database": "MiXeD_AtTaChEd"
            }),
            "mixed_attached",
            "attached_rows",
            1,
            1,
        ),
    ] {
        if let Some(result) = call_case!(case, "sample", args) {
            if let Some(payload) = record_object_response(
                &mut failures,
                case,
                &result,
                database,
                &[
                    "resolved_database",
                    "row_count",
                    "rows",
                    "sample_size",
                    "schema",
                    "stats",
                    "table",
                ],
            ) {
                if payload["table"] != serde_json::json!(table)
                    || payload["row_count"] != serde_json::json!(row_count)
                    || payload["sample_size"] != serde_json::json!(sample_size)
                    || payload["rows"].as_array().map_or(usize::MAX, Vec::len)
                        != usize::try_from(sample_size).expect("sample size is non-negative")
                    || payload["schema"].as_array().map_or(0, Vec::len) == 0
                    || payload["stats"]["operation"] != serde_json::json!("sample")
                {
                    failures.push(format!("{case}: legacy sample fields changed"));
                }
            }
        }
    }

    // Describe's table-specific and populated-listing variants are both
    // successes; the empty listing was pinned above before fixture setup.
    for (case, args, database, expected_table, expected_count) in [
        (
            "describe local listing",
            serde_json::json!({}),
            "local",
            None,
            Some(2),
        ),
        (
            "describe persistent table",
            serde_json::json!({ "table": "persistent_empty", "database": "PERSISTENT" }),
            "persistent",
            Some("persistent_empty"),
            Some(1),
        ),
        (
            "describe attached mixed case",
            serde_json::json!({ "table": "attached_rows", "database": "MiXeD_AtTaChEd" }),
            "mixed_attached",
            Some("attached_rows"),
            Some(1),
        ),
    ] {
        if let Some(result) = call_case!(case, "describe", args) {
            if let Some(payload) = record_object_response(
                &mut failures,
                case,
                &result,
                database,
                &["resolved_database", "tables"],
            ) {
                let tables = payload["tables"].as_array();
                if tables.map_or(false, |tables| tables.len() != expected_count.unwrap_or(0)) {
                    failures.push(format!(
                        "{case}: describe table count changed: got {tables:?}"
                    ));
                }
                if let Some(expected_table) = expected_table {
                    if tables
                        .and_then(|tables| tables.first())
                        .and_then(|table| table.get("name"))
                        != Some(&serde_json::json!(expected_table))
                    {
                        failures.push(format!(
                            "{case}: describe must preserve table name {expected_table}"
                        ));
                    }
                }
            }
        }
    }

    // Chart is the other custom response: inline keeps image first and stats
    // second; disk-only returns just stats. Neither gains structuredContent.
    if let Some(result) = call_case!(
        "chart local inline",
        "chart",
        serde_json::json!({
            "sql": "SELECT label, x FROM local_rows",
            "chart_type": "bar",
            "x": "label",
            "y": "x"
        })
    ) {
        if result.structured_content.is_some() {
            failures.push("chart local inline: chart must not add structuredContent".into());
        }
        if result.content.len() != 2
            || result
                .content
                .first()
                .and_then(|content| content.raw.as_image())
                .is_none()
            || result
                .content
                .get(1)
                .and_then(|content| content.raw.as_text())
                .is_none()
        {
            failures.push(
                "chart local inline: content must remain image first, stats text second".into(),
            );
        }
        let stats = result
            .content
            .get(1)
            .and_then(|content| content.raw.as_text())
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content.text).ok());
        let expected_fields = [
            "bytes",
            "elapsed_ms",
            "format",
            "height",
            "inline",
            "operation",
            "resolved_database",
            "rows_plotted",
            "width",
        ];
        let mut fields = stats
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .map(|object| object.keys().map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        fields.sort_unstable();
        if fields != expected_fields
            || stats.as_ref().and_then(|stats| stats.get("operation"))
                != Some(&serde_json::json!("chart"))
            || stats.as_ref().and_then(|stats| stats.get("inline"))
                != Some(&serde_json::json!(true))
            || stats
                .as_ref()
                .and_then(|stats| stats.get("resolved_database"))
                != Some(&serde_json::json!("local"))
        {
            failures.push(format!(
                "chart local inline: legacy stats or resolved database changed: {stats:?}"
            ));
        }
    }
    if let Some(result) = call_case!(
        "chart persistent disk only",
        "chart",
        serde_json::json!({
            "sql": "SELECT 1 AS x, 2 AS y",
            "chart_type": "bar",
            "x": "x",
            "y": "y",
            "format": "svg",
            "inline": false,
            "database": "PERSISTENT"
        })
    ) {
        if result.structured_content.is_some()
            || result.content.len() != 1
            || result
                .content
                .first()
                .and_then(|content| content.raw.as_text())
                .is_none()
        {
            failures.push(
                "chart persistent disk only: content must remain one stats text block".into(),
            );
        }
        let stats = result
            .content
            .first()
            .and_then(|content| content.raw.as_text())
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content.text).ok());
        let expected_fields = [
            "bytes",
            "elapsed_ms",
            "format",
            "height",
            "inline",
            "operation",
            "output_path",
            "resolved_database",
            "rows_plotted",
            "width",
        ];
        let mut fields = stats
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .map(|object| object.keys().map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        fields.sort_unstable();
        if fields != expected_fields
            || stats.as_ref().and_then(|stats| stats.get("operation"))
                != Some(&serde_json::json!("chart"))
            || stats.as_ref().and_then(|stats| stats.get("format"))
                != Some(&serde_json::json!("svg"))
            || stats.as_ref().and_then(|stats| stats.get("inline"))
                != Some(&serde_json::json!(false))
            || stats
                .as_ref()
                .and_then(|stats| stats.get("resolved_database"))
                != Some(&serde_json::json!("persistent"))
            || stats
                .as_ref()
                .and_then(|stats| stats.get("output_path"))
                .and_then(serde_json::Value::as_str)
                .is_none()
        {
            failures.push(format!(
                "chart persistent disk only: legacy stats or resolved database changed: {stats:?}"
            ));
        }
    }

    if let Err(error) = h.shutdown().await {
        failures.push(format!("test harness shutdown failed: {error}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "resolved_database query success-shape regressions:\n- {}",
            failures.join("\n- ")
        )
        .into())
    }
}
