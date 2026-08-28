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

use base64::Engine as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientInfo, ResourceUpdatedNotificationParam,
    SubscribeRequestParams,
};
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

/// Resource notifications captured by the in-memory client. The aggregate
/// routed-response tests use these to pin the mutation side effects that must
/// survive additive `resolved_database` metadata.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum NotificationEvent {
    ResourceUpdated(String),
    ResourceListChanged,
}

/// Minimal client handler that also records resource notifications emitted by
/// the server after successful mutations.
#[derive(Debug, Clone)]
struct DummyClientHandler {
    notification_tx: tokio::sync::mpsc::UnboundedSender<NotificationEvent>,
}

impl ClientHandler for DummyClientHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) {
        let _ = self
            .notification_tx
            .send(NotificationEvent::ResourceUpdated(params.uri));
    }

    async fn on_resource_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) {
        let _ = self
            .notification_tx
            .send(NotificationEvent::ResourceListChanged);
    }
}

/// In-memory client+server pair backed by a `tokio::io::duplex`.
struct TestHarness {
    client: RunningService<RoleClient, DummyClientHandler>,
    server_handle: tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    /// Shared engine handle retained for status-lock regression scenarios.
    engine_handle: Arc<Mutex<Option<Engine>>>,
    notification_rx: tokio::sync::mpsc::UnboundedReceiver<NotificationEvent>,
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

        let (notification_tx, notification_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = DummyClientHandler { notification_tx }
            .serve(client_io)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

        Ok(Self {
            client,
            server_handle,
            engine_handle,
            notification_rx,
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
        let (notification_tx, notification_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = DummyClientHandler { notification_tx }
            .serve(client_io)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

        Ok(Self {
            client,
            server_handle,
            engine_handle,
            notification_rx,
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
fn record_legacy_object_response(
    failures: &mut Vec<String>,
    case: &str,
    result: &CallToolResult,
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
    Some(payload)
}

fn record_object_response(
    failures: &mut Vec<String>,
    case: &str,
    result: &CallToolResult,
    expected_database: &str,
    expected_fields: &[&str],
) -> Option<serde_json::Value> {
    let payload = record_legacy_object_response(failures, case, result, expected_fields)?;
    if payload.get("resolved_database") != Some(&serde_json::json!(expected_database)) {
        failures.push(format!(
            "{case}: resolved_database must be {expected_database:?}, got {:?}",
            payload.get("resolved_database")
        ));
    }
    Some(payload)
}

fn record_fields(
    failures: &mut Vec<String>,
    case: &str,
    value: &serde_json::Value,
    expected_fields: &[&str],
) {
    let Some(object) = value.as_object() else {
        failures.push(format!("{case}: expected a JSON object, got {value}"));
        return;
    };
    let mut actual: Vec<_> = object.keys().map(String::as_str).collect();
    actual.sort_unstable();
    let mut expected = expected_fields.to_vec();
    expected.sort_unstable();
    if actual != expected {
        failures.push(format!(
            "{case}: fields changed: expected {expected:?}, got {actual:?}"
        ));
    }
}

/// Pin the common success envelope plus the legacy ingest payload and stats.
fn record_ingest_response(
    failures: &mut Vec<String>,
    case: &str,
    result: &CallToolResult,
    expected_database: &str,
    expected_rows: u64,
    expected_table: &str,
    expected_operation: &str,
    expected_format: &str,
    expected_schema_columns: usize,
    schema_changed: bool,
) -> Option<serde_json::Value> {
    let payload = record_object_response(
        failures,
        case,
        result,
        expected_database,
        &["resolved_database", "rows", "schema", "stats"],
    )?;
    if payload["rows"] != serde_json::json!(expected_rows) {
        failures.push(format!(
            "{case}: rows changed: expected {expected_rows}, got {:?}",
            payload.get("rows")
        ));
    }
    let schema = payload["schema"].as_array();
    if schema.map(Vec::len) != Some(expected_schema_columns) {
        failures.push(format!(
            "{case}: schema column count changed: expected {expected_schema_columns}, got {:?}",
            schema.map(Vec::len)
        ));
    }
    if let Some(schema) = schema {
        for (index, column) in schema.iter().enumerate() {
            record_fields(
                failures,
                &format!("{case} schema column {index}"),
                column,
                &["name", "nullable", "type"],
            );
        }
    }

    let mut expected_stats = vec![
        "bytes_read",
        "bytes_stored",
        "compression_ratio",
        "elapsed_ms",
        "file_format",
        "ingest_throughput_mb_sec",
        "operation",
        "rows",
        "rows_per_sec",
        "schema_inference_ms",
        "table",
    ];
    if schema_changed {
        expected_stats.push("schema_changed");
    }
    record_fields(
        failures,
        &format!("{case} stats"),
        &payload["stats"],
        &expected_stats,
    );
    let stats = &payload["stats"];
    if stats["rows"] != serde_json::json!(expected_rows)
        || stats["table"] != serde_json::json!(expected_table)
        || stats["operation"] != serde_json::json!(expected_operation)
        || stats["file_format"] != serde_json::json!(expected_format)
        || stats["schema_changed"]
            != if schema_changed {
                serde_json::json!(true)
            } else {
                serde_json::Value::Null
            }
    {
        failures.push(format!(
            "{case}: legacy ingest stats changed: expected rows={expected_rows}, table={expected_table:?}, operation={expected_operation:?}, format={expected_format:?}, schema_changed={schema_changed}; got {stats}"
        ));
    }
    Some(payload)
}

fn record_copy_response(
    failures: &mut Vec<String>,
    case: &str,
    result: &CallToolResult,
    expected_database: &str,
    expected_table: &str,
    expected_mode: &str,
    expected_rows: i64,
) {
    if let Some(payload) = record_object_response(
        failures,
        case,
        result,
        expected_database,
        &[
            "mode",
            "resolved_database",
            "row_count",
            "stats",
            "target_database",
            "target_table",
        ],
    ) {
        if payload["target_database"] != serde_json::json!(expected_database)
            || payload["target_database"] != payload["resolved_database"]
            || payload["target_table"] != serde_json::json!(expected_table)
            || payload["mode"] != serde_json::json!(expected_mode)
            || payload["row_count"] != serde_json::json!(expected_rows)
        {
            failures.push(format!(
                "{case}: legacy copy result changed or target_database disagrees with resolved_database: {payload}"
            ));
        }
        record_fields(
            failures,
            &format!("{case} stats"),
            &payload["stats"],
            &["elapsed_ms", "operation"],
        );
        if payload["stats"]["operation"] != serde_json::json!("copy_query") {
            failures.push(format!("{case}: stats.operation must remain copy_query"));
        }
    }
}

async fn record_notifications(
    failures: &mut Vec<String>,
    case: &str,
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<NotificationEvent>,
    expected: &[NotificationEvent],
) {
    let mut actual = Vec::with_capacity(expected.len());
    for _ in 0..expected.len() {
        match tokio::time::timeout(Duration::from_secs(2), receiver.recv()).await {
            Ok(Some(event)) => actual.push(event),
            Ok(None) => {
                failures.push(format!("{case}: notification channel closed early"));
                break;
            }
            Err(_) => {
                failures.push(format!(
                    "{case}: timed out waiting for {} notification(s); received {actual:?}",
                    expected.len()
                ));
                break;
            }
        }
    }
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    if actual != expected {
        failures.push(format!(
            "{case}: resource notifications changed: expected {expected:?}, got {actual:?}"
        ));
    }
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
    expected_rows: &serde_json::Value,
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
    if object.get("result") != Some(expected_rows) {
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

/// Pin the MCP error envelope, not just prose: `isError`, structuredContent,
/// and the compatibility text block must all carry the requested code.
fn record_error_contract(
    failures: &mut Vec<String>,
    case: &str,
    result: &CallToolResult,
    expected_code: &str,
) {
    if !is_error(result) {
        failures.push(format!("{case}: expected an MCP tool error, got success"));
        return;
    }
    if result.content.len() != 1 {
        failures.push(format!(
            "{case}: error must contain exactly one compatibility text block, got {}",
            result.content.len()
        ));
    }
    let Some(structured) = result.structured_content.as_ref() else {
        failures.push(format!("{case}: error is missing structuredContent"));
        return;
    };
    if structured
        .pointer("/error/code")
        .and_then(|value| value.as_str())
        != Some(expected_code)
    {
        failures.push(format!(
            "{case}: expected structured error code {expected_code}, got {structured}"
        ));
    }
    let text_payload = first_text(result)
        .as_deref()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
    if text_payload.as_ref() != Some(structured) {
        failures.push(format!(
            "{case}: text error payload must mirror structuredContent exactly"
        ));
    }
}

fn schema_allows_type(schema: &serde_json::Value, expected: &str) -> bool {
    match schema.get("type") {
        Some(serde_json::Value::String(actual)) => actual == expected,
        Some(serde_json::Value::Array(actual)) => actual.iter().any(|value| value == expected),
        _ => false,
    }
}

fn svg_text_opening_tag<'a>(svg: &'a str, exact_text: &str) -> Option<&'a str> {
    let lines: Vec<_> = svg.lines().collect();
    lines.windows(2).find_map(|window| {
        (window[0].starts_with("<text ") && window[1].trim() == exact_text).then_some(window[0])
    })
}

fn inline_image_bytes(
    failures: &mut Vec<String>,
    case: &str,
    result: &CallToolResult,
) -> Option<(String, Vec<u8>)> {
    if is_error(result) {
        failures.push(format!(
            "{case}: expected chart success, got {:?}",
            first_text(result)
        ));
        return None;
    }
    if result.structured_content.is_some()
        || result.content.len() != 2
        || result
            .content
            .get(1)
            .and_then(|content| content.raw.as_text())
            .is_none()
    {
        failures.push(format!(
            "{case}: chart content must remain image first, stats text second, with no structuredContent"
        ));
    }
    let Some(image) = result
        .content
        .first()
        .and_then(|content| content.raw.as_image())
    else {
        failures.push(format!("{case}: first content block is not an image"));
        return None;
    };
    match base64::engine::general_purpose::STANDARD.decode(&image.data) {
        Ok(bytes) => Some((image.mime_type.clone(), bytes)),
        Err(error) => {
            failures.push(format!("{case}: inline image is not valid base64: {error}"));
            None
        }
    }
}

fn svg_i32_attr(line: &str, name: &str) -> Option<i32> {
    let marker = format!("{name}=\"");
    line.split_once(&marker)?.1.split_once('"')?.0.parse().ok()
}

fn svg_primary_blue_rects(svg: &str) -> Vec<(i32, i32, i32, i32)> {
    svg.lines()
        .filter(|line| line.starts_with("<rect ") && line.contains("fill=\"#1F77B4\""))
        .filter_map(|line| {
            let rect = (
                svg_i32_attr(line, "x")?,
                svg_i32_attr(line, "y")?,
                svg_i32_attr(line, "width")?,
                svg_i32_attr(line, "height")?,
            );
            (rect.2 > 20 && rect.3 > 0).then_some(rect)
        })
        .collect()
}

/// Parse the sole JSON text block emitted by the `status` tool.
fn status_json(result: &CallToolResult) -> serde_json::Value {
    serde_json::from_str(&first_text(result).expect("status must return a text payload"))
        .expect("status payload must be JSON")
}

/// Caller-invalid chart ranges must be rejected before Plotters and retain the
/// structured MCP `INVALID_ARGUMENT` mapping on both axes, including ranges a
/// chart type would otherwise ignore.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chart_mcp_rejects_invalid_ranges() -> TestResult {
    let h = TestHarness::start(false, true).await?;
    let mut failures = Vec::new();
    let sql = "SELECT 1 AS category, 2 AS value UNION ALL SELECT 2, 3";

    for (case, args) in [
        (
            "bar reversed ignored x range",
            serde_json::json!({
                "sql": sql,
                "chart_type": "bar",
                "x": "category",
                "y": "value",
                "x_range": [2.0, 1.0],
                "format": "svg"
            }),
        ),
        (
            "bar equal y range",
            serde_json::json!({
                "sql": sql,
                "chart_type": "bar",
                "x": "category",
                "y": "value",
                "y_range": [2.0, 2.0],
                "format": "svg"
            }),
        ),
    ] {
        let result = call_tool(&h.client, "chart", args).await?;
        record_error_contract(&mut failures, case, &result, "INVALID_ARGUMENT");
    }

    h.shutdown().await?;
    assert!(
        failures.is_empty(),
        "invalid chart range MCP failures:\n{}",
        failures.join("\n")
    );
    Ok(())
}

/// A non-finite value returned by a real SQL DOUBLE expression is numeric
/// input that the renderer cannot plot. It must retain the caller-invalid
/// `INVALID_ARGUMENT` envelope rather than being converted to JSON null and
/// misreported as a column schema problem.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chart_mcp_rejects_sql_non_finite_double() -> TestResult {
    let h = TestHarness::start(false, true).await?;
    let mut failures = Vec::new();
    let result = call_tool(
        &h.client,
        "chart",
        serde_json::json!({
            "sql": "SELECT 'not-a-number' AS category, CAST('NaN' AS DOUBLE PRECISION) AS value",
            "chart_type": "bar",
            "x": "category",
            "y": "value",
            "format": "svg"
        }),
    )
    .await?;
    record_error_contract(
        &mut failures,
        "SQL non-finite DOUBLE",
        &result,
        "INVALID_ARGUMENT",
    );

    let histogram_result = call_tool(
        &h.client,
        "chart",
        serde_json::json!({
            "sql": "SELECT CAST(1.0 AS DOUBLE PRECISION) AS value UNION ALL SELECT CAST('NaN' AS DOUBLE PRECISION)",
            "chart_type": "histogram",
            "x": "value",
            "format": "svg"
        }),
    )
    .await?;
    record_error_contract(
        &mut failures,
        "mixed finite/non-finite DOUBLE histogram",
        &histogram_result,
        "INVALID_ARGUMENT",
    );

    h.shutdown().await?;
    assert!(
        failures.is_empty(),
        "SQL non-finite DOUBLE chart failures:\n{}",
        failures.join("\n")
    );
    Ok(())
}

/// Presentation controls are a private renderer extension surfaced only via
/// MCP. This pins their generated schema, omission defaults, structured invalid
/// combinations, image/stats ordering, and both SVG semantics and PNG delivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chart_mcp_presentation_options_contract() -> TestResult {
    let h = TestHarness::start(false, true).await?;
    let mut failures = Vec::new();

    let tools = h.client.list_all_tools().await?;
    let chart_tool = tools.iter().find(|tool| tool.name.as_ref() == "chart");
    match chart_tool {
        Some(tool) => {
            let properties = tool
                .input_schema
                .get("properties")
                .and_then(serde_json::Value::as_object);
            let required = tool
                .input_schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (name, expected_type) in [
                ("bar_orientation", "string"),
                ("label_values", "boolean"),
                ("show_legend", "boolean"),
            ] {
                match properties.and_then(|properties| properties.get(name)) {
                    Some(schema) if schema_allows_type(schema, expected_type) => {}
                    Some(schema) => failures.push(format!(
                        "chart schema property {name} must allow {expected_type}, got {schema}"
                    )),
                    None => {
                        failures.push(format!("chart schema is missing optional property {name}"));
                    }
                }
                if required.iter().any(|value| value == name) {
                    failures.push(format!("chart schema property {name} must remain optional"));
                }
            }
            let orientation_enum = properties
                .and_then(|properties| properties.get("bar_orientation"))
                .and_then(|schema| schema.get("enum"))
                .and_then(serde_json::Value::as_array);
            let accepts_vertical = orientation_enum
                .is_some_and(|values| values.iter().any(|value| value == "vertical"));
            let accepts_horizontal = orientation_enum
                .is_some_and(|values| values.iter().any(|value| value == "horizontal"));
            if !accepts_vertical || !accepts_horizontal {
                failures.push(format!(
                    "bar_orientation schema must enumerate vertical and horizontal, got {orientation_enum:?}"
                ));
            }
        }
        None => failures.push("generated catalog is missing chart tool".into()),
    }

    let sql = "SELECT 'North' AS category, 137 AS value, 'Legend alpha' AS series \
               UNION ALL SELECT 'South', 251, 'Legend beta'";
    let default_result = call_tool(
        &h.client,
        "chart",
        serde_json::json!({
            "sql": sql,
            "chart_type": "bar",
            "x": "category",
            "y": "value",
            "series": "series",
            "format": "svg",
            "width": 520,
            "height": 360
        }),
    )
    .await?;
    if let Some((mime, bytes)) = inline_image_bytes(
        &mut failures,
        "omitted presentation defaults",
        &default_result,
    ) {
        if mime != "image/svg+xml" {
            failures.push(format!("default SVG MIME changed: {mime}"));
        }
        match String::from_utf8(bytes) {
            Ok(svg) => {
                let category_tag = svg_text_opening_tag(&svg, "category");
                let value_tag = svg_text_opening_tag(&svg, "value");
                let category_axis_invalid = match category_tag {
                    Some(tag) => tag.contains("rotate(270"),
                    None => true,
                };
                let value_axis_invalid = match value_tag {
                    Some(tag) => !tag.contains("rotate(270"),
                    None => true,
                };
                if category_axis_invalid || value_axis_invalid {
                    failures.push(
                        "omitted bar_orientation must retain vertical category-x/value-y axes"
                            .into(),
                    );
                }
                if !svg.contains("Legend alpha") || !svg.contains("Legend beta") {
                    failures.push("omitted show_legend must default to true".into());
                }
                if svg.lines().any(|line| line.trim() == "137")
                    || svg.lines().any(|line| line.trim() == "251")
                {
                    failures.push("omitted label_values must default to false".into());
                }
            }
            Err(error) => failures.push(format!("default SVG is not UTF-8: {error}")),
        }
    }

    let horizontal_result = call_tool(
        &h.client,
        "chart",
        serde_json::json!({
            "sql": sql,
            "chart_type": "bar",
            "x": "category",
            "y": "value",
            "series": "series",
            "format": "svg",
            "bar_orientation": "horizontal",
            "label_values": true,
            "show_legend": false,
            "width": 520,
            "height": 360
        }),
    )
    .await?;
    if let Some((_, bytes)) = inline_image_bytes(
        &mut failures,
        "explicit horizontal presentation",
        &horizontal_result,
    ) {
        match String::from_utf8(bytes) {
            Ok(svg) => {
                let category_tag = svg_text_opening_tag(&svg, "category");
                let value_tag = svg_text_opening_tag(&svg, "value");
                let category_axis_invalid = match category_tag {
                    Some(tag) => !tag.contains("rotate(270"),
                    None => true,
                };
                let value_axis_invalid = match value_tag {
                    Some(tag) => tag.contains("rotate(270"),
                    None => true,
                };
                if category_axis_invalid || value_axis_invalid {
                    failures.push(
                        "horizontal bars must swap to category-y/value-x axis descriptions".into(),
                    );
                }
                for exact in ["137", "251"] {
                    if !svg.lines().any(|line| line.trim() == exact) {
                        failures.push(format!(
                            "label_values:true must render exact scalar {exact}"
                        ));
                    }
                }
                if svg.contains("Legend alpha") || svg.contains("Legend beta") {
                    failures.push("show_legend:false must remove series legend text".into());
                }
            }
            Err(error) => failures.push(format!("horizontal SVG is not UTF-8: {error}")),
        }
    }

    let png_result = call_tool(
        &h.client,
        "chart",
        serde_json::json!({
            "sql": sql,
            "chart_type": "bar",
            "x": "category",
            "y": "value",
            "series": "series",
            "format": "png",
            "bar_orientation": "horizontal",
            "label_values": true,
            "show_legend": false
        }),
    )
    .await?;
    if let Some((mime, bytes)) = inline_image_bytes(&mut failures, "horizontal PNG", &png_result) {
        if mime != "image/png"
            || !bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
        {
            failures.push(format!(
                "horizontal PNG must carry PNG MIME/magic, got {mime} and {:?}",
                bytes.get(..8)
            ));
        }
    }

    for (case, args) in [
        (
            "label_values on line",
            serde_json::json!({
                "sql": sql,
                "chart_type": "line",
                "x": "value",
                "y": "value",
                "label_values": true,
                "format": "svg"
            }),
        ),
        (
            "explicit vertical bar orientation on scatter",
            serde_json::json!({
                "sql": sql,
                "chart_type": "scatter",
                "x": "value",
                "y": "value",
                "bar_orientation": "vertical",
                "format": "svg"
            }),
        ),
        (
            "unknown bar orientation",
            serde_json::json!({
                "sql": sql,
                "chart_type": "bar",
                "x": "category",
                "y": "value",
                "bar_orientation": "diagonal",
                "format": "svg"
            }),
        ),
    ] {
        let result = call_tool(&h.client, "chart", args).await?;
        record_error_contract(&mut failures, case, &result, "INVALID_ARGUMENT");
    }

    h.shutdown().await?;
    assert!(
        failures.is_empty(),
        "chart presentation MCP failures:\n{}",
        failures.join("\n")
    );
    Ok(())
}

/// NUMERIC values beyond f64's exact-integer boundary must retain their SQL
/// scalar spelling all the way through the MCP query materializer and the
/// renderer's value-label path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chart_mcp_preserves_high_precision_numeric_value_label() -> TestResult {
    const EXACT_VALUE: &str = "9007199254740993";

    let h = TestHarness::start(false, true).await?;
    let mut failures = Vec::new();
    let result = call_tool(
        &h.client,
        "chart",
        serde_json::json!({
            "sql": "SELECT 'precise' AS category, CAST(9007199254740993 AS NUMERIC(16,0)) AS value",
            "chart_type": "bar",
            "x": "category",
            "y": "value",
            "format": "svg",
            "label_values": true,
            "show_legend": false,
            "width": 520,
            "height": 360
        }),
    )
    .await?;

    if let Some((mime, bytes)) =
        inline_image_bytes(&mut failures, "high-precision NUMERIC label", &result)
    {
        if mime != "image/svg+xml" {
            failures.push(format!("high-precision chart MIME changed: {mime}"));
        }
        match String::from_utf8(bytes) {
            Ok(svg) if svg_text_opening_tag(&svg, EXACT_VALUE).is_some() => {}
            Ok(svg) => {
                let lines: Vec<_> = svg.lines().collect();
                let numeric_text: Vec<_> = lines
                    .windows(2)
                    .filter(|window| window[0].starts_with("<text "))
                    .map(|window| window[1].trim())
                    .filter(|text| text.chars().any(|character| character.is_ascii_digit()))
                    .collect();
                failures.push(format!(
                    "label_values:true must preserve exact SQL NUMERIC {EXACT_VALUE}; numeric SVG text was {numeric_text:?}"
                ));
            }
            Err(error) => failures.push(format!(
                "high-precision NUMERIC chart SVG is not UTF-8: {error}"
            )),
        }
    }

    h.shutdown().await?;
    assert!(
        failures.is_empty(),
        "high-precision NUMERIC chart failures:\n{}",
        failures.join("\n")
    );
    Ok(())
}

/// The logarithmic measure scale remains MCP-only and defaults to linear. Pin
/// its schema/parser, positive-domain validation, real log geometry, structured
/// errors, and unchanged inline SVG/PNG content ordering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chart_mcp_log_scale_contract() -> TestResult {
    let h = TestHarness::start(false, true).await?;
    let mut failures = Vec::new();

    let tools = h.client.list_all_tools().await?;
    let y_scale_schema = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "chart")
        .and_then(|tool| tool.input_schema.get("properties"))
        .and_then(serde_json::Value::as_object)
        .and_then(|properties| properties.get("y_scale"));
    match y_scale_schema {
        Some(schema) if schema_allows_type(schema, "string") => {
            let values = schema.get("enum").and_then(serde_json::Value::as_array);
            if !values.is_some_and(|values| {
                values.iter().any(|value| value == "linear")
                    && values.iter().any(|value| value == "log")
            }) {
                failures.push(format!(
                    "y_scale schema must enumerate linear and log, got {schema}"
                ));
            }
        }
        Some(schema) => failures.push(format!(
            "optional y_scale schema must allow string, got {schema}"
        )),
        None => failures.push("chart schema is missing optional y_scale".into()),
    }
    let y_scale_required = tools
        .iter()
        .find(|tool| tool.name.as_ref() == "chart")
        .and_then(|tool| tool.input_schema.get("required"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|required| required.iter().any(|value| value == "y_scale"));
    if y_scale_required {
        failures.push("y_scale must remain optional so omission defaults to linear".into());
    }

    let linear_default = call_tool(
        &h.client,
        "chart",
        serde_json::json!({
            "sql": "SELECT 'negative' AS category, -10 AS value UNION ALL SELECT 'zero', 0 UNION ALL SELECT 'positive', 10",
            "chart_type": "bar",
            "x": "category",
            "y": "value",
            "format": "svg"
        }),
    )
    .await?;
    if inline_image_bytes(
        &mut failures,
        "omitted y_scale linear default",
        &linear_default,
    )
    .is_none()
    {
        failures.push("omitted y_scale must continue accepting zero/negative linear data".into());
    }

    let positive_sql = "SELECT 'ten' AS category, 10 AS value UNION ALL SELECT 'hundred', 100";
    let log_svg_result = call_tool(
        &h.client,
        "chart",
        serde_json::json!({
            "sql": positive_sql,
            "chart_type": "bar",
            "x": "category",
            "y": "value",
            "format": "svg",
            "y_scale": "log",
            "y_range": [1.0, 1000.0],
            "width": 520,
            "height": 360
        }),
    )
    .await?;
    if let Some((mime, bytes)) =
        inline_image_bytes(&mut failures, "positive log SVG", &log_svg_result)
    {
        if mime != "image/svg+xml" {
            failures.push(format!("log SVG MIME changed: {mime}"));
        }
        match String::from_utf8(bytes) {
            Ok(svg) => {
                let mut heights: Vec<_> = svg_primary_blue_rects(&svg)
                    .into_iter()
                    .map(|rect| rect.3)
                    .collect();
                heights.sort_unstable();
                match heights.as_slice() {
                    [short, tall] if *short > 0 => {
                        let ratio = f64::from(*tall) / f64::from(*short);
                        if !(1.7..=2.3).contains(&ratio) {
                            failures.push(format!(
                                "10 and 100 over log range 1..1000 must occupy one and two decades (about 2x heights), got {heights:?} ratio={ratio}"
                            ));
                        }
                    }
                    _ => failures.push(format!(
                        "positive log SVG must contain two visible primary bars, got heights {heights:?}"
                    )),
                }
            }
            Err(error) => failures.push(format!("log SVG is not UTF-8: {error}")),
        }
    }

    let log_png_result = call_tool(
        &h.client,
        "chart",
        serde_json::json!({
            "sql": positive_sql,
            "chart_type": "bar",
            "x": "category",
            "y": "value",
            "format": "png",
            "y_scale": "log",
            "y_range": [1.0, 1000.0]
        }),
    )
    .await?;
    if let Some((mime, bytes)) =
        inline_image_bytes(&mut failures, "positive log PNG", &log_png_result)
    {
        if mime != "image/png"
            || !bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
        {
            failures.push(format!(
                "log PNG must carry PNG MIME/magic, got {mime} and {:?}",
                bytes.get(..8)
            ));
        }
    }

    for (case, args) in [
        (
            "unknown scale",
            serde_json::json!({
                "sql": positive_sql, "chart_type": "bar", "x": "category", "y": "value",
                "y_scale": "symlog", "format": "svg"
            }),
        ),
        (
            "zero log value",
            serde_json::json!({
                "sql": "SELECT 'zero' AS category, 0 AS value", "chart_type": "bar",
                "x": "category", "y": "value", "y_scale": "log", "format": "svg"
            }),
        ),
        (
            "negative log value",
            serde_json::json!({
                "sql": "SELECT 1 AS category, -1 AS value", "chart_type": "line",
                "x": "category", "y": "value", "y_scale": "log", "format": "svg"
            }),
        ),
        (
            "mixed-sign log values",
            serde_json::json!({
                "sql": "SELECT 1 AS category, -1 AS value UNION ALL SELECT 2, 1",
                "chart_type": "scatter", "x": "category", "y": "value",
                "y_scale": "log", "format": "svg"
            }),
        ),
        (
            "log histogram",
            serde_json::json!({
                "sql": "SELECT 10 AS value", "chart_type": "histogram", "x": "value",
                "y_scale": "log", "format": "svg"
            }),
        ),
        (
            "log range excludes plotted value",
            serde_json::json!({
                "sql": positive_sql, "chart_type": "bar", "x": "category", "y": "value",
                "y_scale": "log", "y_range": [20.0, 200.0], "format": "svg"
            }),
        ),
    ] {
        let result = call_tool(&h.client, "chart", args).await?;
        record_error_contract(&mut failures, case, &result, "INVALID_ARGUMENT");
    }

    h.shutdown().await?;
    assert!(
        failures.is_empty(),
        "chart log-scale MCP failures:\n{}",
        failures.join("\n")
    );
    Ok(())
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
    // `diagnostic` is the JSON-serialized error text, so on Windows the
    // path's backslashes are JSON-escaped (`\` -> `\\`) and a raw `contains`
    // against the un-escaped path misses. Match the JSON-escaped form; on
    // Unix the path has no backslashes, so `escaped_path` equals the raw path
    // and the check is unchanged there.
    let escaped_path = effective_path.to_str().unwrap().replace('\\', "\\\\");
    assert!(
        diagnostic.contains(&escaped_path),
        "must name exact effective persistent path {} (JSON-escaped: {escaped_path}): {diagnostic}",
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

    let engine_lock_holder = hold_engine_lock(Arc::clone(&h.engine_handle));
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

    drop(engine_lock_holder);
    h.shutdown().await
}

/// The status fast path must not wait behind an in-flight data-plane lock and
/// must honestly omit fields that require that lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_degraded_returns_promptly_while_engine_locked() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let engine_lock_holder = hold_engine_lock(Arc::clone(&h.engine_handle));

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

    drop(engine_lock_holder);
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
            &serde_json::json!([{ "x": 1 }]),
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
            &serde_json::json!([]),
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
            &serde_json::json!([{ "x": 7 }]),
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
                if tables.is_some_and(|tables| tables.len() != expected_count.unwrap_or(0)) {
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

/// Every successful ingest/export/watch/catalog response keeps its legacy
/// shape while reporting the canonical database selected by routing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolved_database_data_success_shapes() -> TestResult {
    let mut h = TestHarness::start(false, false).await?;
    let temp = TempDir::new()?;
    let watch_dir = TempDir::new()?;
    let attached_path = temp.path().join("mixed-data.hyper");
    let base_csv = temp.path().join("base.csv");
    let merge_add_csv = temp.path().join("merge-add.csv");
    let merge_same_csv = temp.path().join("merge-same.csv");
    let two_row_batch_csv = temp.path().join("batch-a.csv");
    let single_row_batch_csv = temp.path().join("batch-b.csv");
    std::fs::write(&base_csv, b"id,name\n1,alice\n2,bob\n")?;
    std::fs::write(&merge_add_csv, b"id,name,extra\n1,alicia,new\n")?;
    std::fs::write(&merge_same_csv, b"id,name,extra\n2,robert,same\n")?;
    std::fs::write(&two_row_batch_csv, b"id,value\n1,a\n2,b\n")?;
    std::fs::write(&single_row_batch_csv, b"id,value\n3,c\n")?;
    let mut failures = Vec::new();

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

    if let Some(result) = call_case!(
        "setup mixed-case data attachment",
        "attach_database",
        serde_json::json!({
            "alias": "MiXeD_Data",
            "kind": "local_file",
            "path": attached_path.to_string_lossy(),
            "writable": true,
            "on_missing": "create"
        })
    ) {
        if is_error(&result) {
            failures.push(format!(
                "setup mixed-case data attachment: {:?}",
                first_text(&result)
            ));
        }
    }

    if let Err(error) = h
        .client
        .subscribe(SubscribeRequestParams::new("hyper://workspace"))
        .await
    {
        failures.push(format!(
            "setup workspace resource subscription failed: {error}"
        ));
    }

    if let Some(result) = call_case!(
        "load_data local replace",
        "load_data",
        serde_json::json!({
            "table": "inline_local",
            "format": "json",
            "data": r#"[{"id":1,"name":"one"},{"id":2,"name":"two"}]"#
        })
    ) {
        record_ingest_response(
            &mut failures,
            "load_data local replace",
            &result,
            "local",
            2,
            "inline_local",
            "load_data",
            "json",
            2,
            false,
        );
        record_notifications(
            &mut failures,
            "load_data local replace",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "load_data explicit local wins over persist",
        "load_data",
        serde_json::json!({
            "table": "inline_local",
            "format": "json",
            "mode": "append",
            "database": "LoCaL",
            "persist": true,
            "data": r#"[{"id":3,"name":"three"}]"#
        })
    ) {
        record_ingest_response(
            &mut failures,
            "load_data explicit local wins over persist",
            &result,
            "local",
            1,
            "inline_local",
            "load_data",
            "json",
            2,
            false,
        );
        record_notifications(
            &mut failures,
            "load_data explicit local wins over persist",
            &mut h.notification_rx,
            &[NotificationEvent::ResourceUpdated(
                "hyper://workspace".into(),
            )],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "load_file persist true replace",
        "load_file",
        serde_json::json!({
            "path": base_csv.to_string_lossy(),
            "table": "file_persistent",
            "format": "csv",
            "persist": true
        })
    ) {
        record_ingest_response(
            &mut failures,
            "load_file persist true replace",
            &result,
            "persistent",
            2,
            "file_persistent",
            "load_file",
            "csv",
            2,
            false,
        );
        record_notifications(
            &mut failures,
            "load_file persist true replace",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "load_file attached replace",
        "load_file",
        serde_json::json!({
            "path": base_csv.to_string_lossy(),
            "table": "file_attached",
            "format": "csv",
            "database": "MiXeD_DaTa"
        })
    ) {
        record_ingest_response(
            &mut failures,
            "load_file attached replace",
            &result,
            "mixed_data",
            2,
            "file_attached",
            "load_file",
            "csv",
            2,
            false,
        );
        record_notifications(
            &mut failures,
            "load_file attached replace",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "load_file attached merge adds schema",
        "load_file",
        serde_json::json!({
            "path": merge_add_csv.to_string_lossy(),
            "table": "file_attached",
            "format": "csv",
            "mode": "merge",
            "merge_key": ["id"],
            "database": "MIXED_DATA"
        })
    ) {
        record_ingest_response(
            &mut failures,
            "load_file attached merge adds schema",
            &result,
            "mixed_data",
            1,
            "file_attached",
            "load_file",
            "csv",
            3,
            true,
        );
        record_notifications(
            &mut failures,
            "load_file attached merge adds schema",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "load_file attached merge preserves schema",
        "load_file",
        serde_json::json!({
            "path": merge_same_csv.to_string_lossy(),
            "table": "file_attached",
            "format": "csv",
            "mode": "merge",
            "merge_key": ["id"],
            "database": "mixed_data"
        })
    ) {
        record_ingest_response(
            &mut failures,
            "load_file attached merge preserves schema",
            &result,
            "mixed_data",
            1,
            "file_attached",
            "load_file",
            "csv",
            3,
            false,
        );
        record_notifications(
            &mut failures,
            "load_file attached merge preserves schema",
            &mut h.notification_rx,
            &[NotificationEvent::ResourceUpdated(
                "hyper://workspace".into(),
            )],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "load_files all success local precedence",
        "load_files",
        serde_json::json!({
            "files": [
                {"path": two_row_batch_csv.to_string_lossy(), "table": "batch_local_a", "format": "csv"},
                {"path": single_row_batch_csv.to_string_lossy(), "table": "batch_local_b", "format": "csv"}
            ],
            "concurrency": 2,
            "database": "LOCAL",
            "persist": true
        })
    ) {
        if let Some(payload) = record_object_response(
            &mut failures,
            "load_files all success local precedence",
            &result,
            "local",
            &["resolved_database", "results", "summary"],
        ) {
            record_fields(
                &mut failures,
                "load_files all success local precedence summary",
                &payload["summary"],
                &["concurrency", "failed", "succeeded", "total"],
            );
            if payload["summary"]
                != serde_json::json!({"total": 2, "succeeded": 2, "failed": 0, "concurrency": 2})
            {
                failures.push(format!(
                    "load_files all success local precedence: summary changed: {}",
                    payload["summary"]
                ));
            }
            let results = payload["results"].as_array();
            if results.map(Vec::len) != Some(2) {
                failures.push(format!(
                    "load_files all success local precedence: expected two results, got {results:?}"
                ));
            }
            if let Some(results) = results {
                for (index, (table, rows)) in [("batch_local_a", 2), ("batch_local_b", 1)]
                    .into_iter()
                    .enumerate()
                {
                    if let Some(entry) = results.get(index) {
                        record_fields(
                            &mut failures,
                            &format!("load_files all success local precedence result {index}"),
                            entry,
                            &["rows", "schema", "stats", "table"],
                        );
                        if entry["table"] != serde_json::json!(table)
                            || entry["rows"] != serde_json::json!(rows)
                            || entry.get("resolved_database").is_some()
                        {
                            failures.push(format!(
                                "load_files all success local precedence result {index}: legacy entry changed or duplicated resolved_database: {entry}"
                            ));
                        }
                    }
                }
            }
        }
        record_notifications(
            &mut failures,
            "load_files all success local precedence",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "load_files partial per-file success",
        "load_files",
        serde_json::json!({
            "files": [
                {"path": single_row_batch_csv.to_string_lossy(), "table": "batch_partial_ok", "format": "csv"},
                {"path": two_row_batch_csv.to_string_lossy(), "table": "batch_partial_bad", "format": "csv", "schema": 42}
            ],
            "concurrency": 2,
            "persist": true
        })
    ) {
        if let Some(payload) = record_object_response(
            &mut failures,
            "load_files partial per-file success",
            &result,
            "persistent",
            &["resolved_database", "results", "summary"],
        ) {
            if payload["summary"]
                != serde_json::json!({"total": 2, "succeeded": 1, "failed": 1, "concurrency": 2})
            {
                failures.push(format!(
                    "load_files partial per-file success: summary changed: {}",
                    payload["summary"]
                ));
            }
            let results = payload["results"].as_array();
            if let Some(results) = results {
                if let Some(success) = results.first() {
                    record_fields(
                        &mut failures,
                        "load_files partial per-file success result",
                        success,
                        &["rows", "schema", "stats", "table"],
                    );
                    if success["table"] != serde_json::json!("batch_partial_ok")
                        || success["rows"] != serde_json::json!(1)
                        || success.get("resolved_database").is_some()
                    {
                        failures.push(format!(
                            "load_files partial per-file success: successful entry changed: {success}"
                        ));
                    }
                }
                if let Some(failed) = results.get(1) {
                    record_fields(
                        &mut failures,
                        "load_files partial per-file failure result",
                        failed,
                        &["error", "table"],
                    );
                    record_fields(
                        &mut failures,
                        "load_files partial per-file failure error",
                        &failed["error"],
                        &["code", "message"],
                    );
                    if failed["table"] != serde_json::json!("batch_partial_bad")
                        || failed["error"]["code"] != serde_json::json!("SchemaMismatch")
                        || failed.get("resolved_database").is_some()
                    {
                        failures.push(format!(
                            "load_files partial per-file success: failed entry changed: {failed}"
                        ));
                    }
                }
            } else {
                failures.push(format!(
                    "load_files partial per-file success: results must be an array: {}",
                    payload["results"]
                ));
            }
        }
        record_notifications(
            &mut failures,
            "load_files partial per-file success",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "load_files all entries failed remains top-level success",
        "load_files",
        serde_json::json!({
            "files": [
                {"path": two_row_batch_csv.to_string_lossy(), "table": "batch_all_bad", "format": "csv", "schema": 42}
            ],
            "concurrency": 1,
            "database": "MiXeD_DaTa"
        })
    ) {
        if let Some(payload) = record_object_response(
            &mut failures,
            "load_files all entries failed remains top-level success",
            &result,
            "mixed_data",
            &["resolved_database", "results", "summary"],
        ) {
            if payload["summary"]
                != serde_json::json!({"total": 1, "succeeded": 0, "failed": 1, "concurrency": 1})
                || payload["results"].as_array().map(Vec::len) != Some(1)
                || payload["results"][0]["error"]["code"] != serde_json::json!("SchemaMismatch")
            {
                failures.push(format!(
                    "load_files all entries failed remains top-level success: legacy batch shape changed: {payload}"
                ));
            }
        }
    }

    let canonical_watch_dir = match watch_dir.path().canonicalize() {
        Ok(path) => Some(path),
        Err(error) => {
            failures.push(format!("watch directory canonicalization failed: {error}"));
            None
        }
    };
    if let Some(result) = call_case!(
        "watch_directory attached empty initial sweep",
        "watch_directory",
        serde_json::json!({
            "path": watch_dir.path().to_string_lossy(),
            "table": "file_attached",
            "database": "MiXeD_DaTa",
            "max_concurrent": 1
        })
    ) {
        if let Some(payload) = record_object_response(
            &mut failures,
            "watch_directory attached empty initial sweep",
            &result,
            "mixed_data",
            &[
                "directory",
                "initial_sweep",
                "max_concurrent",
                "resolved_database",
                "status",
                "table",
            ],
        ) {
            record_fields(
                &mut failures,
                "watch_directory attached empty initial sweep stats",
                &payload["initial_sweep"],
                &["files_failed", "files_ingested"],
            );
            if payload["directory"]
                != serde_json::json!(canonical_watch_dir
                    .as_deref()
                    .unwrap_or_else(|| watch_dir.path())
                    .to_string_lossy())
                || payload["table"] != serde_json::json!("file_attached")
                || payload["status"] != serde_json::json!("watching")
                || payload["max_concurrent"] != serde_json::json!(1)
                || payload["initial_sweep"]
                    != serde_json::json!({"files_ingested": 0, "files_failed": 0})
            {
                failures.push(format!(
                    "watch_directory attached empty initial sweep: watcher handle changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "watch_directory registry status",
        "status",
        serde_json::json!({})
    ) {
        let payload = first_text(&result)
            .as_deref()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
        let watcher = payload
            .as_ref()
            .and_then(|body| body["watchers"].as_array())
            .and_then(|watchers| watchers.first());
        if let Some(watcher) = watcher {
            record_fields(
                &mut failures,
                "watch_directory registry status entry",
                watcher,
                &[
                    "directory",
                    "files_failed",
                    "files_ingested",
                    "in_flight",
                    "last_error",
                    "last_event_ms_ago",
                    "max_concurrent",
                    "table",
                    "target_db",
                ],
            );
            if watcher["target_db"] != serde_json::json!("mixed_data")
                || watcher["table"] != serde_json::json!("file_attached")
                || watcher["files_ingested"] != serde_json::json!(0)
                || watcher["files_failed"] != serde_json::json!(0)
                || watcher["in_flight"] != serde_json::json!(0)
            {
                failures.push(format!(
                    "watch_directory registry status: active watcher state changed: {watcher}"
                ));
            }
        } else {
            failures.push(format!(
                "watch_directory registry status: expected one active watcher, got {payload:?}"
            ));
        }
    }

    if let Some(path) = canonical_watch_dir.as_ref() {
        if let Some(result) = call_case!(
            "watch_directory teardown",
            "unwatch_directory",
            serde_json::json!({"path": path.to_string_lossy()})
        ) {
            if let Some(payload) = record_legacy_object_response(
                &mut failures,
                "watch_directory teardown",
                &result,
                &[
                    "directory",
                    "files_failed",
                    "files_ingested",
                    "last_error",
                    "status",
                    "table",
                ],
            ) {
                if payload["status"] != serde_json::json!("stopped")
                    || payload["table"] != serde_json::json!("file_attached")
                    || payload["files_ingested"] != serde_json::json!(0)
                    || payload["files_failed"] != serde_json::json!(0)
                {
                    failures.push(format!(
                        "watch_directory teardown: legacy stop summary changed: {payload}"
                    ));
                }
            }
        }
    }

    if let Some(result) = call_case!(
        "watch_directory registry empty after teardown",
        "status",
        serde_json::json!({})
    ) {
        let payload = first_text(&result)
            .as_deref()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
        if match payload
            .as_ref()
            .and_then(|body| body["watchers"].as_array())
        {
            Some(watchers) => !watchers.is_empty(),
            None => true,
        } {
            failures.push(format!(
                "watch_directory registry empty after teardown: watcher handle leaked: {payload:?}"
            ));
        }
    }

    let csv_export_path = temp.path().join("persistent.csv");
    if let Some(result) = call_case!(
        "export persistent table to csv",
        "export",
        serde_json::json!({
            "table": "file_persistent",
            "path": csv_export_path.to_string_lossy(),
            "format": "csv",
            "database": "PERSISTENT"
        })
    ) {
        if let Some(payload) = record_object_response(
            &mut failures,
            "export persistent table to csv",
            &result,
            "persistent",
            &[
                "file_size_bytes",
                "output_path",
                "resolved_database",
                "rows",
                "stats",
            ],
        ) {
            record_fields(
                &mut failures,
                "export persistent table to csv stats",
                &payload["stats"],
                &[
                    "elapsed_ms",
                    "file_size_bytes",
                    "format",
                    "operation",
                    "output_path",
                    "rows",
                    "rows_per_sec",
                ],
            );
            if payload["rows"] != serde_json::json!(2)
                || payload["output_path"] != serde_json::json!(csv_export_path.to_string_lossy())
                || match payload["file_size_bytes"].as_u64() {
                    Some(bytes) => bytes == 0,
                    None => true,
                }
                || payload["stats"]["operation"] != serde_json::json!("export")
                || payload["stats"]["format"] != serde_json::json!("csv")
            {
                failures.push(format!(
                    "export persistent table to csv: legacy export payload changed: {payload}"
                ));
            }
        }
        if !csv_export_path.is_file() {
            failures.push("export persistent table to csv: output file was not created".into());
        }
    }

    let hyper_export_path = temp.path().join("local.hyper");
    if let Some(result) = call_case!(
        "export bare local hyper snapshot",
        "export",
        serde_json::json!({
            "path": hyper_export_path.to_string_lossy(),
            "format": "hyper",
            "database": "LOCAL"
        })
    ) {
        if let Some(payload) = record_object_response(
            &mut failures,
            "export bare local hyper snapshot",
            &result,
            "local",
            &[
                "file_size_bytes",
                "output_path",
                "resolved_database",
                "rows",
                "stats",
            ],
        ) {
            if payload["rows"] != serde_json::json!(0)
                || payload["stats"]["format"] != serde_json::json!("hyper")
                || payload["output_path"] != serde_json::json!(hyper_export_path.to_string_lossy())
            {
                failures.push(format!(
                    "export bare local hyper snapshot: legacy snapshot payload changed: {payload}"
                ));
            }
        }
        if !hyper_export_path.is_file() {
            failures.push("export bare local hyper snapshot: output file was not created".into());
        }
    }

    if let Some(result) = call_case!(
        "set_table_metadata attached catalog entry",
        "set_table_metadata",
        serde_json::json!({
            "table": "file_attached",
            "database": "MiXeD_DaTa",
            "purpose": "resolved database regression",
            "license": "CC0",
            "notes": "legacy fields stay intact"
        })
    ) {
        if let Some(payload) = record_object_response(
            &mut failures,
            "set_table_metadata attached catalog entry",
            &result,
            "mixed_data",
            &[
                "created_by",
                "data_url",
                "last_modified_by",
                "last_refreshed_at",
                "license",
                "load_params",
                "load_tool",
                "loaded_at",
                "notes",
                "purpose",
                "resolved_database",
                "row_count",
                "source_description",
                "source_url",
                "table_name",
            ],
        ) {
            if payload["table_name"] != serde_json::json!("file_attached")
                || payload["purpose"] != serde_json::json!("resolved database regression")
                || payload["license"] != serde_json::json!("CC0")
                || payload["notes"] != serde_json::json!("legacy fields stay intact")
                || payload["load_tool"] != serde_json::json!("load_file")
                || payload["row_count"] != serde_json::json!(1)
                || payload["loaded_at"].as_str().is_none()
                || payload["last_refreshed_at"].as_str().is_none()
            {
                failures.push(format!(
                    "set_table_metadata attached catalog entry: legacy catalog entry changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "catalog routing side effects",
        "query",
        serde_json::json!({
            "sql": "SELECT table_name, load_tool FROM _table_catalog WHERE table_name = 'file_attached'",
            "database": "mixed_data"
        })
    ) {
        let text = all_text(&result);
        if is_error(&result) || !text.contains("file_attached") || !text.contains("load_file") {
            failures.push(format!(
                "catalog routing side effects: attached catalog stub missing or changed: {text}"
            ));
        }
    }

    let mut unexpected_notifications = Vec::new();
    while let Ok(notification) = h.notification_rx.try_recv() {
        unexpected_notifications.push(notification);
    }
    if !unexpected_notifications.is_empty() {
        failures.push(format!(
            "non-mutating data cases emitted unexpected resource notifications: {unexpected_notifications:?}"
        ));
    }

    if let Err(error) = h.shutdown().await {
        failures.push(format!("test harness shutdown failed: {error}"));
    }
    assert!(
        failures.is_empty(),
        "resolved_database data success-shape regressions:\n- {}",
        failures.join("\n- ")
    );
    Ok(())
}

/// `copy_query` keeps its legacy `target_database` compatibility field and
/// makes it identical to the common canonical routing metadata for every mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_query_preserves_target_and_resolved_database() -> TestResult {
    let mut h = TestHarness::start(false, false).await?;
    let temp = TempDir::new()?;
    let attached_path = temp.path().join("copy-target.hyper");
    let mut failures = Vec::new();

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

    if let Some(result) = call_case!(
        "setup mixed-case copy target",
        "attach_database",
        serde_json::json!({
            "alias": "MiXeD_Copy",
            "kind": "local_file",
            "path": attached_path.to_string_lossy(),
            "writable": true,
            "on_missing": "create"
        })
    ) {
        if is_error(&result) {
            failures.push(format!(
                "setup mixed-case copy target: {:?}",
                first_text(&result)
            ));
        }
    }
    if let Err(error) = h
        .client
        .subscribe(SubscribeRequestParams::new("hyper://workspace"))
        .await
    {
        failures.push(format!("setup copy resource subscription failed: {error}"));
    }

    if let Some(result) = call_case!(
        "copy create local default",
        "copy_query",
        serde_json::json!({
            "mode": "create",
            "target_table": "copy_local",
            "sql": "SELECT 1 AS x"
        })
    ) {
        record_copy_response(
            &mut failures,
            "copy create local default",
            &result,
            "local",
            "copy_local",
            "create",
            1,
        );
        record_notifications(
            &mut failures,
            "copy create local default",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "copy append explicit local",
        "copy_query",
        serde_json::json!({
            "mode": "append",
            "target_database": "LoCaL",
            "target_table": "copy_local",
            "sql": "SELECT 2 AS x"
        })
    ) {
        record_copy_response(
            &mut failures,
            "copy append explicit local",
            &result,
            "local",
            "copy_local",
            "append",
            2,
        );
        record_notifications(
            &mut failures,
            "copy append explicit local",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "copy replace local",
        "copy_query",
        serde_json::json!({
            "mode": "replace",
            "target_database": "LOCAL",
            "target_table": "copy_local",
            "sql": "SELECT 3 AS x UNION ALL SELECT 4 AS x"
        })
    ) {
        record_copy_response(
            &mut failures,
            "copy replace local",
            &result,
            "local",
            "copy_local",
            "replace",
            2,
        );
        record_notifications(
            &mut failures,
            "copy replace local",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "copy create attached canonical alias",
        "copy_query",
        serde_json::json!({
            "mode": "create",
            "target_database": "MiXeD_CoPy",
            "target_table": "copy_attached",
            "sql": "SELECT 10 AS x"
        })
    ) {
        record_copy_response(
            &mut failures,
            "copy create attached canonical alias",
            &result,
            "mixed_copy",
            "copy_attached",
            "create",
            1,
        );
        record_notifications(
            &mut failures,
            "copy create attached canonical alias",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "copy append attached canonical alias",
        "copy_query",
        serde_json::json!({
            "mode": "append",
            "target_database": "MIXED_COPY",
            "target_table": "copy_attached",
            "sql": "SELECT 11 AS x"
        })
    ) {
        record_copy_response(
            &mut failures,
            "copy append attached canonical alias",
            &result,
            "mixed_copy",
            "copy_attached",
            "append",
            2,
        );
        record_notifications(
            &mut failures,
            "copy append attached canonical alias",
            &mut h.notification_rx,
            &[NotificationEvent::ResourceUpdated(
                "hyper://workspace".into(),
            )],
        )
        .await;
    }

    if let Some(result) = call_case!(
        "copy replace attached canonical alias",
        "copy_query",
        serde_json::json!({
            "mode": "replace",
            "target_database": "mixed_copy",
            "target_table": "copy_attached",
            "sql": "SELECT 12 AS x UNION ALL SELECT 13 AS x"
        })
    ) {
        record_copy_response(
            &mut failures,
            "copy replace attached canonical alias",
            &result,
            "mixed_copy",
            "copy_attached",
            "replace",
            2,
        );
        record_notifications(
            &mut failures,
            "copy replace attached canonical alias",
            &mut h.notification_rx,
            &[
                NotificationEvent::ResourceUpdated("hyper://workspace".into()),
                NotificationEvent::ResourceListChanged,
            ],
        )
        .await;
    }

    for (case, sql, database) in [
        (
            "copy local target contents",
            "SELECT COUNT(*) AS n FROM copy_local",
            None,
        ),
        (
            "copy attached target contents",
            "SELECT COUNT(*) AS n FROM copy_attached",
            Some("mixed_copy"),
        ),
    ] {
        let args = match database {
            Some(database) => serde_json::json!({"sql": sql, "database": database}),
            None => serde_json::json!({"sql": sql}),
        };
        if let Some(result) = call_case!(case, "query", args) {
            let text = all_text(&result);
            if is_error(&result) || (!text.contains("\"n\":2") && !text.contains("\"n\": 2")) {
                failures.push(format!(
                    "{case}: copied table must contain the two replace rows; got {text}"
                ));
            }
        }
    }

    let mut unexpected_notifications = Vec::new();
    while let Ok(notification) = h.notification_rx.try_recv() {
        unexpected_notifications.push(notification);
    }
    if !unexpected_notifications.is_empty() {
        failures.push(format!(
            "copy_query emitted unexpected extra resource notifications: {unexpected_notifications:?}"
        ));
    }
    if let Err(error) = h.shutdown().await {
        failures.push(format!("test harness shutdown failed: {error}"));
    }
    assert!(
        failures.is_empty(),
        "copy_query resolved-database compatibility regressions:\n- {}",
        failures.join("\n- ")
    );
    Ok(())
}
