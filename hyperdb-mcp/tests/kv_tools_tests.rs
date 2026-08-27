// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! End-to-end coverage for the `kv_*` scratchpad tools.
//!
//! Like [`end_to_end_mcp_tests`], these spin up a `HyperMcpServer` and a
//! minimal client on opposite halves of an in-memory `tokio::io::duplex`
//! pair, then invoke the tools through the real rmcp dispatch path
//! (params deserialization → handler → `CallToolResult`). This exercises
//! the handlers exactly as an MCP client would, including the
//! `database`/`persist` routing, the read-only guard, and durability.

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientInfo};
use rmcp::service::{RoleClient, RunningService};
use rmcp::{ClientHandler, ServiceExt};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

use hyperdb_mcp::server::HyperMcpServer;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Minimal client handler — only exists to satisfy `ServiceExt`.
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
    /// Held so the workspace temp dir outlives the server. The persistence
    /// test drives the reopen path via [`start_at`](TestHarness::start_at)
    /// with an explicit path instead of reading this back.
    _temp_dir: Arc<TempDir>,
}

impl TestHarness {
    /// Spin up a server with a fresh persistent workspace + an in-memory
    /// client. `read_only=false` is the typical case; `ephemeral_only=true`
    /// skips the persistent attachment.
    async fn start(
        read_only: bool,
        ephemeral_only: bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let temp_dir = Arc::new(TempDir::new()?);
        let persistent_path = temp_dir.path().join("workspace.hyper");
        Self::start_at(read_only, ephemeral_only, persistent_path, temp_dir).await
    }

    /// Like [`start`](Self::start) but reuses a caller-provided workspace
    /// path + temp dir, so a second server can reopen the same on-disk file
    /// (used by the persistence-across-restart test).
    async fn start_at(
        read_only: bool,
        ephemeral_only: bool,
        persistent_path: PathBuf,
        temp_dir: Arc<TempDir>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
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

/// Invoke a tool by name, building request params from a JSON object.
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

/// First text-content block of a tool result (used for error messages).
fn first_text(result: &CallToolResult) -> Option<String> {
    result
        .content
        .first()
        .and_then(|c| c.raw.as_text())
        .map(|t| t.text.clone())
}

/// Did the tool return an `is_error: true` result?
fn is_error(result: &CallToolResult) -> bool {
    result.is_error.unwrap_or(false)
}

/// The tool's JSON payload as a `serde_json::Value`. Every `kv_*` handler
/// returns via `ok_content`, which serializes the body into the single
/// text content block (pretty-printed) as well as `structuredContent`.
/// Parsing the text block keeps this robust across rmcp versions (the
/// `structured_content` field type varies) and matches how the sibling
/// e2e tests read tool output.
fn structured(result: &CallToolResult) -> serde_json::Value {
    let text = first_text(result).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
}

/// Record one KV success without panicking so the aggregate routing test can
/// reach every later tool/branch before its single final assertion.
fn record_kv_response(
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

// =====================================================================
// Core CRUD lifecycle (default / ephemeral database).
// =====================================================================

/// set → get round-trips; get on an absent key returns `{found:false,
/// value:null}` (not an error); overwrite returns the new value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_get_roundtrip_and_overwrite() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    // Absent key first — must be a clean miss, not an error.
    let miss = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "cfg", "key": "theme" }),
    )
    .await?;
    assert!(
        !is_error(&miss),
        "kv_get miss must not error: {:?}",
        first_text(&miss)
    );
    assert_eq!(structured(&miss)["found"], serde_json::json!(false));
    assert_eq!(structured(&miss)["value"], serde_json::Value::Null);

    // Set, then read back.
    let set = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "cfg", "key": "theme", "value": "dark" }),
    )
    .await?;
    assert!(!is_error(&set), "kv_set failed: {:?}", first_text(&set));
    assert_eq!(structured(&set)["stored"], serde_json::json!(true));

    let got = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "cfg", "key": "theme" }),
    )
    .await?;
    assert_eq!(structured(&got)["found"], serde_json::json!(true));
    assert_eq!(structured(&got)["value"], serde_json::json!("dark"));

    // Overwrite (upsert) → new value.
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "cfg", "key": "theme", "value": "light" }),
    )
    .await?;
    let got2 = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "cfg", "key": "theme" }),
    )
    .await?;
    assert_eq!(structured(&got2)["value"], serde_json::json!("light"));

    h.shutdown().await
}

/// list returns keys sorted ascending; size counts them; list_stores
/// includes the store namespace once it holds data.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_list_size_and_list_stores() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    for (k, v) in [("gamma", "3"), ("alpha", "1"), ("beta", "2")] {
        call_tool(
            &h.client,
            "kv_set",
            serde_json::json!({ "store": "s", "key": k, "value": v }),
        )
        .await?;
    }

    let list = call_tool(&h.client, "kv_list", serde_json::json!({ "store": "s" })).await?;
    assert_eq!(structured(&list)["count"], serde_json::json!(3));
    assert_eq!(
        structured(&list)["keys"],
        serde_json::json!(["alpha", "beta", "gamma"]),
        "keys must be sorted ascending"
    );

    let size = call_tool(&h.client, "kv_size", serde_json::json!({ "store": "s" })).await?;
    assert_eq!(structured(&size)["size"], serde_json::json!(3));

    let stores = call_tool(&h.client, "kv_list_stores", serde_json::json!({})).await?;
    let names = structured(&stores)["stores"].clone();
    assert!(
        names.as_array().is_some_and(|a| a.iter().any(|n| n == "s")),
        "list_stores must include 's'; got: {names}"
    );

    h.shutdown().await
}

/// kv_list default (values absent/false) preserves the keys-only shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_list_keys_only_unchanged() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "a", "value": "v1" }),
    )
    .await?;
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "b", "value": "v2" }),
    )
    .await?;
    let list = call_tool(&h.client, "kv_list", serde_json::json!({ "store": "s" })).await?;
    let body = structured(&list);
    assert_eq!(body["store"], serde_json::json!("s"));
    assert_eq!(body["count"], serde_json::json!(2));
    assert_eq!(body["keys"], serde_json::json!(["a", "b"]));
    h.shutdown().await
}

/// kv_list with values:true returns entries with both key and value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_list_values_returns_entries() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "x", "value": "hello" }),
    )
    .await?;
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "y", "value": "world" }),
    )
    .await?;
    let list = call_tool(
        &h.client,
        "kv_list",
        serde_json::json!({ "store": "s", "values": true }),
    )
    .await?;
    let body = structured(&list);
    assert_eq!(body["store"], serde_json::json!("s"));
    let entries = body["entries"]
        .as_array()
        .expect("entries must be an array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["key"], serde_json::json!("x"));
    assert_eq!(entries[0]["value"], serde_json::json!("hello"));
    assert_eq!(entries[1]["key"], serde_json::json!("y"));
    assert_eq!(entries[1]["value"], serde_json::json!("world"));
    h.shutdown().await
}

/// kv_size reports both key count and total value bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_size_reports_bytes() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "a", "value": "abc" }),
    )
    .await?;
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "b", "value": "de" }),
    )
    .await?;

    let size = call_tool(&h.client, "kv_size", serde_json::json!({ "store": "s" })).await?;
    assert_eq!(structured(&size)["size"], serde_json::json!(2), "two keys");
    assert_eq!(
        structured(&size)["bytes"],
        serde_json::json!(5),
        "3+2=5 bytes"
    );
    h.shutdown().await
}

/// delete returns `{deleted:true}` when the key existed and
/// `{deleted:false}` on a second delete (idempotent, not an error).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_delete_reports_whether_key_existed() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "v" }),
    )
    .await?;

    let first = call_tool(
        &h.client,
        "kv_delete",
        serde_json::json!({ "store": "s", "key": "k" }),
    )
    .await?;
    assert_eq!(structured(&first)["deleted"], serde_json::json!(true));

    let second = call_tool(
        &h.client,
        "kv_delete",
        serde_json::json!({ "store": "s", "key": "k" }),
    )
    .await?;
    assert!(!is_error(&second), "second delete must not error");
    assert_eq!(structured(&second)["deleted"], serde_json::json!(false));

    h.shutdown().await
}

/// pop returns the lowest-keyed entry and removes it; on an empty store
/// it returns `{found:false}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_pop_removes_lowest_key_then_empty() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    for (k, v) in [("b", "2"), ("a", "1")] {
        call_tool(
            &h.client,
            "kv_set",
            serde_json::json!({ "store": "q", "key": k, "value": v }),
        )
        .await?;
    }

    let pop1 = call_tool(&h.client, "kv_pop", serde_json::json!({ "store": "q" })).await?;
    assert_eq!(structured(&pop1)["found"], serde_json::json!(true));
    assert_eq!(
        structured(&pop1)["key"],
        serde_json::json!("a"),
        "lowest key first"
    );
    assert_eq!(structured(&pop1)["value"], serde_json::json!("1"));

    let pop2 = call_tool(&h.client, "kv_pop", serde_json::json!({ "store": "q" })).await?;
    assert_eq!(structured(&pop2)["key"], serde_json::json!("b"));

    // Store now empty.
    let pop3 = call_tool(&h.client, "kv_pop", serde_json::json!({ "store": "q" })).await?;
    assert_eq!(structured(&pop3)["found"], serde_json::json!(false));

    h.shutdown().await
}

/// clear returns the number of keys removed; the store is empty afterward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_clear_empties_the_store() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    for k in ["x", "y", "z"] {
        call_tool(
            &h.client,
            "kv_set",
            serde_json::json!({ "store": "s", "key": k, "value": "1" }),
        )
        .await?;
    }

    let cleared = call_tool(&h.client, "kv_clear", serde_json::json!({ "store": "s" })).await?;
    assert_eq!(structured(&cleared)["removed"], serde_json::json!(3));

    let size = call_tool(&h.client, "kv_size", serde_json::json!({ "store": "s" })).await?;
    assert_eq!(structured(&size)["size"], serde_json::json!(0));

    h.shutdown().await
}

// =====================================================================
// Database routing + isolation.
// =====================================================================

/// A value written to the persistent database is invisible from the
/// default (ephemeral) database, and vice-versa. `kv_list_stores` routes
/// per-database too (proves `kv_list_stores_in`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_database_routing_isolates_stores() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    // Write into persistent.
    let set = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "persisted", "database": "persistent" }),
    )
    .await?;
    assert!(
        !is_error(&set),
        "kv_set to persistent failed: {:?}",
        first_text(&set)
    );

    // Visible when reading persistent.
    let got_persist = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "k", "database": "persistent" }),
    )
    .await?;
    assert_eq!(
        structured(&got_persist)["value"],
        serde_json::json!("persisted")
    );

    // NOT visible from the default (ephemeral) database.
    let got_default = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "k" }),
    )
    .await?;
    assert_eq!(
        structured(&got_default)["found"],
        serde_json::json!(false),
        "ephemeral DB must not see the persistent store's value"
    );

    // list_stores is per-database: persistent has 's', default has none.
    let stores_persist = call_tool(
        &h.client,
        "kv_list_stores",
        serde_json::json!({ "database": "persistent" }),
    )
    .await?;
    assert!(
        structured(&stores_persist)["stores"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n == "s")),
        "persistent list_stores must include 's'"
    );

    let stores_default = call_tool(&h.client, "kv_list_stores", serde_json::json!({})).await?;
    assert_eq!(
        structured(&stores_default)["count"],
        serde_json::json!(0),
        "default DB must have no stores"
    );

    h.shutdown().await
}

/// `persist:true` routes to the persistent database, equivalent to
/// `database:"persistent"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_persist_flag_routes_to_persistent() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "v", "persist": true }),
    )
    .await?;

    // Readable via database:"persistent" (proving persist:true == persistent).
    let got = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "k", "database": "persistent" }),
    )
    .await?;
    assert_eq!(structured(&got)["value"], serde_json::json!("v"));

    h.shutdown().await
}

/// A store written into a writable *attached* database is reachable via
/// that alias and invisible from the default database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_routes_to_attached_database() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    // Attach a fresh writable .hyper under alias "aux".
    let aux_dir = TempDir::new()?;
    let aux_path = aux_dir.path().join("aux.hyper");
    let attach = call_tool(
        &h.client,
        "attach_database",
        serde_json::json!({
            "alias": "aux",
            "kind": "local_file",
            "path": aux_path.to_string_lossy(),
            "writable": true,
            "on_missing": "create",
        }),
    )
    .await?;
    assert!(
        !is_error(&attach),
        "attach failed: {:?}",
        first_text(&attach)
    );

    // Write into the attached DB and read it back via the same alias.
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "in_aux", "database": "aux" }),
    )
    .await?;
    let got_aux = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "k", "database": "aux" }),
    )
    .await?;
    assert_eq!(structured(&got_aux)["value"], serde_json::json!("in_aux"));

    // Invisible from the default database.
    let got_default = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "k" }),
    )
    .await?;
    assert_eq!(structured(&got_default)["found"], serde_json::json!(false));

    h.shutdown().await
}

// =====================================================================
// Guard rails: ephemeral-only + read-only server.
// =====================================================================

/// On an ephemeral-only server, `kv_set` with `database:"persistent"`
/// returns an error content (not a panic) naming the cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_persistent_on_ephemeral_only_errors() -> TestResult {
    let h = TestHarness::start(false, true).await?;

    let result = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "v", "database": "persistent" }),
    )
    .await?;
    assert!(
        is_error(&result),
        "must reject persistent in ephemeral-only mode"
    );
    let msg = first_text(&result).unwrap_or_default();
    assert!(
        msg.contains("ephemeral-only") || msg.contains("persistent"),
        "error must name the cause; got: {msg}"
    );

    h.shutdown().await
}

/// On a `--read-only` server the mutating KV tools are blocked by
/// `check_writable`, while the readers still work against the writable
/// target (they issue only `CREATE TABLE IF NOT EXISTS` + SELECT). This
/// settles the create-on-open question empirically: a reader opening a
/// store in a read-only *server* against a writable engine target
/// succeeds, so readers are intentionally NOT gated by `check_writable`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_read_only_server_blocks_mutators_allows_readers() -> TestResult {
    let h = TestHarness::start(true, false).await?;

    // Mutators are blocked with a read-only violation.
    for (tool, args) in [
        (
            "kv_set",
            serde_json::json!({ "store": "s", "key": "k", "value": "v" }),
        ),
        ("kv_delete", serde_json::json!({ "store": "s", "key": "k" })),
        ("kv_pop", serde_json::json!({ "store": "s" })),
        ("kv_clear", serde_json::json!({ "store": "s" })),
    ] {
        let r = call_tool(&h.client, tool, args).await?;
        assert!(is_error(&r), "{tool} must be blocked in read-only mode");
        let msg = first_text(&r).unwrap_or_default();
        assert!(
            msg.contains("read-only"),
            "{tool} error must mention read-only mode; got: {msg}"
        );
    }

    // Readers succeed (create-on-open is a no-op CREATE IF NOT EXISTS that
    // the writable engine target accepts even under a read-only server).
    let got = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "missing" }),
    )
    .await?;
    assert!(
        !is_error(&got),
        "kv_get must work on a read-only server: {:?}",
        first_text(&got)
    );
    assert_eq!(structured(&got)["found"], serde_json::json!(false));

    let list = call_tool(&h.client, "kv_list", serde_json::json!({ "store": "s" })).await?;
    assert!(
        !is_error(&list),
        "kv_list must work: {:?}",
        first_text(&list)
    );
    assert_eq!(structured(&list)["count"], serde_json::json!(0));

    let size = call_tool(&h.client, "kv_size", serde_json::json!({ "store": "s" })).await?;
    assert!(
        !is_error(&size),
        "kv_size must work: {:?}",
        first_text(&size)
    );
    assert_eq!(structured(&size)["size"], serde_json::json!(0));

    h.shutdown().await
}

// =====================================================================
// Durability: persistent values survive a server restart.
// =====================================================================

/// A value written with `database:"persistent"` survives dropping the
/// server and reopening a fresh one against the same workspace file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_persistent_value_survives_server_restart() -> TestResult {
    let temp_dir = Arc::new(TempDir::new()?);
    let workspace = temp_dir.path().join("workspace.hyper");

    // First server: write into persistent, then shut down cleanly.
    {
        let h =
            TestHarness::start_at(false, false, workspace.clone(), Arc::clone(&temp_dir)).await?;
        let set = call_tool(
            &h.client,
            "kv_set",
            serde_json::json!({ "store": "s", "key": "k", "value": "durable", "database": "persistent" }),
        )
        .await?;
        assert!(!is_error(&set), "kv_set failed: {:?}", first_text(&set));
        h.shutdown().await?;
    }

    // Second server on the same workspace path: the value is still there.
    let h2 = TestHarness::start_at(false, false, workspace, Arc::clone(&temp_dir)).await?;
    let got = call_tool(
        &h2.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "k", "database": "persistent" }),
    )
    .await?;
    assert_eq!(
        structured(&got)["value"],
        serde_json::json!("durable"),
        "persistent value must survive a server restart"
    );

    h2.shutdown().await
}

/// kv_set reports `created` (insert vs overwrite) and `value_bytes`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_reports_created_and_bytes() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let first = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "hello" }),
    )
    .await?;
    assert_eq!(structured(&first)["created"], serde_json::json!(true));
    assert_eq!(structured(&first)["value_bytes"], serde_json::json!(5));

    let second = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "hi" }),
    )
    .await?;
    assert_eq!(structured(&second)["created"], serde_json::json!(false));
    assert_eq!(structured(&second)["value_bytes"], serde_json::json!(2));
    h.shutdown().await
}

/// kv_set warns when value_bytes exceeds the soft limit (1MB).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_large_value_warns() -> TestResult {
    let h = TestHarness::start(false, false).await?;

    // At exactly 1 MB boundary: no warning
    let one_mb = "x".repeat(1_048_576);
    let at_limit = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "at_limit", "value": one_mb }),
    )
    .await?;
    assert_eq!(structured(&at_limit)["stored"], serde_json::json!(true));
    assert!(structured(&at_limit)["warning"].is_null());

    // Above 1 MB: warning fires
    let over_mb = "x".repeat(1_048_577);
    let over_limit = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "over_limit", "value": over_mb }),
    )
    .await?;
    let structured_over = structured(&over_limit);
    assert_eq!(structured_over["stored"], serde_json::json!(true));
    let warning = structured_over["warning"]
        .as_str()
        .expect("warning should be a string");
    assert!(
        warning.contains("1048576"),
        "warning should mention the byte limit; got: {warning}"
    );
    assert!(
        warning.contains("soft limit") || warning.contains("recommended"),
        "warning should mention soft limit or recommended; got: {warning}"
    );

    h.shutdown().await
}

/// overwrite:false skips an existing key without clobbering it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_overwrite_false_guards() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "orig" }),
    )
    .await?;
    let guard = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "new", "overwrite": false }),
    )
    .await?;
    assert_eq!(structured(&guard)["stored"], serde_json::json!(false));
    assert_eq!(structured(&guard)["existed"], serde_json::json!(true));
    let got = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "k" }),
    )
    .await?;
    assert_eq!(structured(&got)["value"], serde_json::json!("orig"));
    h.shutdown().await
}

/// value_path reads a file's contents; neither/both value+value_path errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_value_path_reads_file() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let dir = tempfile::TempDir::new()?;
    let path = dir.path().join("payload.txt");
    std::fs::write(&path, "from-file")?;
    let abs = std::fs::canonicalize(&path)?;

    let set = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "f", "value_path": abs.to_string_lossy() }),
    )
    .await?;
    assert!(
        !is_error(&set),
        "value_path set failed: {:?}",
        first_text(&set)
    );
    let got = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "f" }),
    )
    .await?;
    assert_eq!(structured(&got)["value"], serde_json::json!("from-file"));

    // Neither value nor value_path → INVALID_ARGUMENT.
    let neither = call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "x" }),
    )
    .await?;
    assert!(is_error(&neither));
    // Both → INVALID_ARGUMENT.
    let both = call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "y", "value": "v", "value_path": abs.to_string_lossy() })).await?;
    assert!(is_error(&both));
    h.shutdown().await
}

/// kv_set_many writes all entries atomically (overwrite=true default); reports
/// {stored, created, overwritten, total_bytes}; a mixed batch counts correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_many_writes_all() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "a", "value": "1" }),
    )
    .await?;

    let batch = call_tool(
        &h.client,
        "kv_set_many",
        serde_json::json!({
            "store": "s",
            "entries": [
                { "key": "a", "value": "10" },   // overwrite
                { "key": "b", "value": "20" },   // new
                { "key": "c", "value": "30" },   // new
            ]
        }),
    )
    .await?;
    assert!(
        !is_error(&batch),
        "kv_set_many failed: {:?}",
        first_text(&batch)
    );
    assert_eq!(structured(&batch)["stored"], serde_json::json!(3));
    assert_eq!(structured(&batch)["created"], serde_json::json!(2));
    assert_eq!(structured(&batch)["overwritten"], serde_json::json!(1));
    assert_eq!(
        structured(&batch)["total_bytes"],
        serde_json::json!(6),
        "10+20+30 = 6 bytes"
    );

    let got = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "a" }),
    )
    .await?;
    assert_eq!(structured(&got)["value"], serde_json::json!("10"));
    h.shutdown().await
}

/// kv_set_many with overwrite=false skips existing keys, reports {stored, created, skipped}.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_many_guard_skips_existing() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(
        &h.client,
        "kv_set",
        serde_json::json!({ "store": "s", "key": "a", "value": "orig" }),
    )
    .await?;

    let guard = call_tool(
        &h.client,
        "kv_set_many",
        serde_json::json!({
            "store": "s",
            "entries": [
                { "key": "a", "value": "new" },   // skipped
                { "key": "b", "value": "b1" },    // written
            ],
            "overwrite": false
        }),
    )
    .await?;
    assert!(
        !is_error(&guard),
        "kv_set_many guard failed: {:?}",
        first_text(&guard)
    );
    assert_eq!(structured(&guard)["stored"], serde_json::json!(1));
    assert_eq!(structured(&guard)["created"], serde_json::json!(1));
    assert_eq!(structured(&guard)["skipped"], serde_json::json!(1));
    // total_bytes is the sum of ALL submitted entry values ("new"=3 + "b1"=2),
    // an upper bound under overwrite=false: the batch-guard primitive returns
    // only counts, not which keys were actually written, so total_bytes cannot
    // subtract the skipped entry's bytes.
    assert_eq!(
        structured(&guard)["total_bytes"],
        serde_json::json!(5),
        "\"new\"(3) + \"b1\"(2), all submitted"
    );

    let got = call_tool(
        &h.client,
        "kv_get",
        serde_json::json!({ "store": "s", "key": "a" }),
    )
    .await?;
    assert_eq!(
        structured(&got)["value"],
        serde_json::json!("orig"),
        "existing value untouched"
    );
    h.shutdown().await
}

/// kv_set_many rejects empty entries with INVALID_ARGUMENT.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_many_empty_batch_errors() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let empty = call_tool(
        &h.client,
        "kv_set_many",
        serde_json::json!({
            "store": "s",
            "entries": []
        }),
    )
    .await?;
    assert!(is_error(&empty), "empty entries must error");
    h.shutdown().await
}

/// All nine routed KV tools preserve every legacy success branch while
/// reporting the canonical target selected by database/persist precedence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolved_database_kv_success_shapes() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let attached_dir = TempDir::new()?;
    let attached_path = attached_dir.path().join("mixed-kv.hyper");
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
        "kv_get missing local key",
        "kv_get",
        serde_json::json!({"store": "empty_local", "key": "missing"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_get missing local key",
            &result,
            "local",
            &["found", "resolved_database", "value"],
        ) {
            if payload["found"] != serde_json::json!(false)
                || payload["value"] != serde_json::Value::Null
            {
                failures.push(format!(
                    "kv_get missing local key: legacy miss shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_list empty local keys",
        "kv_list",
        serde_json::json!({"store": "empty_local"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_list empty local keys",
            &result,
            "local",
            &["count", "keys", "resolved_database", "store"],
        ) {
            if payload["store"] != serde_json::json!("empty_local")
                || payload["count"] != serde_json::json!(0)
                || payload["keys"] != serde_json::json!([])
            {
                failures.push(format!(
                    "kv_list empty local keys: legacy empty shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_list empty local entries",
        "kv_list",
        serde_json::json!({"store": "empty_local", "values": true})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_list empty local entries",
            &result,
            "local",
            &["entries", "resolved_database", "store"],
        ) {
            if payload["store"] != serde_json::json!("empty_local")
                || payload["entries"] != serde_json::json!([])
            {
                failures.push(format!(
                    "kv_list empty local entries: legacy empty values shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_list_stores empty local database",
        "kv_list_stores",
        serde_json::json!({})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_list_stores empty local database",
            &result,
            "local",
            &["count", "resolved_database", "stores"],
        ) {
            if payload["count"] != serde_json::json!(0)
                || payload["stores"] != serde_json::json!([])
            {
                failures.push(format!(
                    "kv_list_stores empty local database: legacy empty shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_size empty local store",
        "kv_size",
        serde_json::json!({"store": "empty_local"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_size empty local store",
            &result,
            "local",
            &["bytes", "resolved_database", "size", "store"],
        ) {
            if payload["store"] != serde_json::json!("empty_local")
                || payload["size"] != serde_json::json!(0)
                || payload["bytes"] != serde_json::json!(0)
            {
                failures.push(format!(
                    "kv_size empty local store: legacy empty size changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_pop empty local store",
        "kv_pop",
        serde_json::json!({"store": "empty_local"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_pop empty local store",
            &result,
            "local",
            &["found", "resolved_database"],
        ) {
            if payload["found"] != serde_json::json!(false) {
                failures.push(format!(
                    "kv_pop empty local store: legacy empty pop changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_delete missing local key",
        "kv_delete",
        serde_json::json!({"store": "empty_local", "key": "missing"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_delete missing local key",
            &result,
            "local",
            &["deleted", "key", "resolved_database", "store"],
        ) {
            if payload["deleted"] != serde_json::json!(false)
                || payload["store"] != serde_json::json!("empty_local")
                || payload["key"] != serde_json::json!("missing")
            {
                failures.push(format!(
                    "kv_delete missing local key: legacy idempotent delete changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_clear explicit local wins over persist",
        "kv_clear",
        serde_json::json!({"store": "empty_local", "database": "LoCaL", "persist": true})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_clear explicit local wins over persist",
            &result,
            "local",
            &["removed", "resolved_database", "store"],
        ) {
            if payload["removed"] != serde_json::json!(0)
                || payload["store"] != serde_json::json!("empty_local")
            {
                failures.push(format!(
                    "kv_clear explicit local wins over persist: legacy idempotent clear changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_set persistent insert via persist",
        "kv_set",
        serde_json::json!({
            "store": "persistent_store",
            "key": "p",
            "value": "one",
            "persist": true
        })
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_set persistent insert via persist",
            &result,
            "persistent",
            &[
                "created",
                "key",
                "resolved_database",
                "store",
                "stored",
                "value_bytes",
            ],
        ) {
            if payload["stored"] != serde_json::json!(true)
                || payload["created"] != serde_json::json!(true)
                || payload["store"] != serde_json::json!("persistent_store")
                || payload["key"] != serde_json::json!("p")
                || payload["value_bytes"] != serde_json::json!(3)
            {
                failures.push(format!(
                    "kv_set persistent insert via persist: legacy insert shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_get persistent found",
        "kv_get",
        serde_json::json!({
            "store": "persistent_store",
            "key": "p",
            "database": "PERSISTENT"
        })
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_get persistent found",
            &result,
            "persistent",
            &["found", "resolved_database", "value"],
        ) {
            if payload["found"] != serde_json::json!(true)
                || payload["value"] != serde_json::json!("one")
            {
                failures.push(format!(
                    "kv_get persistent found: legacy hit shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_set persistent overwrite",
        "kv_set",
        serde_json::json!({
            "store": "persistent_store",
            "key": "p",
            "value": "two",
            "database": "Persistent"
        })
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_set persistent overwrite",
            &result,
            "persistent",
            &[
                "created",
                "key",
                "resolved_database",
                "store",
                "stored",
                "value_bytes",
            ],
        ) {
            if payload["stored"] != serde_json::json!(true)
                || payload["created"] != serde_json::json!(false)
                || payload["value_bytes"] != serde_json::json!(3)
            {
                failures.push(format!(
                    "kv_set persistent overwrite: legacy overwrite shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_set persistent guard skip",
        "kv_set",
        serde_json::json!({
            "store": "persistent_store",
            "key": "p",
            "value": "three",
            "overwrite": false,
            "database": "persistent"
        })
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_set persistent guard skip",
            &result,
            "persistent",
            &[
                "created",
                "existed",
                "key",
                "resolved_database",
                "store",
                "stored",
                "value_bytes",
            ],
        ) {
            if payload["stored"] != serde_json::json!(false)
                || payload["created"] != serde_json::json!(false)
                || payload["existed"] != serde_json::json!(true)
                || payload["value_bytes"] != serde_json::json!(5)
            {
                failures.push(format!(
                    "kv_set persistent guard skip: legacy guard shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "setup mixed-case KV attachment",
        "attach_database",
        serde_json::json!({
            "alias": "MiXeD_Kv",
            "kind": "local_file",
            "path": attached_path.to_string_lossy(),
            "writable": true,
            "on_missing": "create"
        })
    ) {
        if is_error(&result) {
            failures.push(format!(
                "setup mixed-case KV attachment: {:?}",
                first_text(&result)
            ));
        }
    }

    if let Some(result) = call_case!(
        "kv_set_many attached create batch",
        "kv_set_many",
        serde_json::json!({
            "store": "routed",
            "entries": [
                {"key": "a", "value": "aa"},
                {"key": "b", "value": "bbb"}
            ],
            "database": "MiXeD_Kv"
        })
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_set_many attached create batch",
            &result,
            "mixed_kv",
            &[
                "created",
                "overwritten",
                "resolved_database",
                "stored",
                "total_bytes",
            ],
        ) {
            if payload["stored"] != serde_json::json!(2)
                || payload["created"] != serde_json::json!(2)
                || payload["overwritten"] != serde_json::json!(0)
                || payload["total_bytes"] != serde_json::json!(5)
            {
                failures.push(format!(
                    "kv_set_many attached create batch: legacy batch shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_set_many attached mixed overwrite batch",
        "kv_set_many",
        serde_json::json!({
            "store": "routed",
            "entries": [
                {"key": "a", "value": "A"},
                {"key": "c", "value": "ccc"}
            ],
            "database": "MIXED_KV"
        })
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_set_many attached mixed overwrite batch",
            &result,
            "mixed_kv",
            &[
                "created",
                "overwritten",
                "resolved_database",
                "stored",
                "total_bytes",
            ],
        ) {
            if payload["stored"] != serde_json::json!(2)
                || payload["created"] != serde_json::json!(1)
                || payload["overwritten"] != serde_json::json!(1)
                || payload["total_bytes"] != serde_json::json!(4)
            {
                failures.push(format!(
                    "kv_set_many attached mixed overwrite batch: legacy batch shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_set_many attached guard batch",
        "kv_set_many",
        serde_json::json!({
            "store": "routed",
            "entries": [
                {"key": "a", "value": "new"},
                {"key": "d", "value": "dddd"}
            ],
            "overwrite": false,
            "database": "mixed_kv"
        })
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_set_many attached guard batch",
            &result,
            "mixed_kv",
            &[
                "created",
                "resolved_database",
                "skipped",
                "stored",
                "total_bytes",
            ],
        ) {
            if payload["stored"] != serde_json::json!(1)
                || payload["created"] != serde_json::json!(1)
                || payload["skipped"] != serde_json::json!(1)
                || payload["total_bytes"] != serde_json::json!(7)
            {
                failures.push(format!(
                    "kv_set_many attached guard batch: legacy guard shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_list attached populated keys",
        "kv_list",
        serde_json::json!({"store": "routed", "database": "MiXeD_Kv"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_list attached populated keys",
            &result,
            "mixed_kv",
            &["count", "keys", "resolved_database", "store"],
        ) {
            if payload["count"] != serde_json::json!(4)
                || payload["keys"] != serde_json::json!(["a", "b", "c", "d"])
            {
                failures.push(format!(
                    "kv_list attached populated keys: sorted keys changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_list attached populated entries",
        "kv_list",
        serde_json::json!({"store": "routed", "values": true, "database": "MIXED_KV"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_list attached populated entries",
            &result,
            "mixed_kv",
            &["entries", "resolved_database", "store"],
        ) {
            if payload["entries"]
                != serde_json::json!([
                    {"key": "a", "value": "A"},
                    {"key": "b", "value": "bbb"},
                    {"key": "c", "value": "ccc"},
                    {"key": "d", "value": "dddd"}
                ])
            {
                failures.push(format!(
                    "kv_list attached populated entries: entry ordering/values changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_list_stores attached populated database",
        "kv_list_stores",
        serde_json::json!({"database": "mixed_kv"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_list_stores attached populated database",
            &result,
            "mixed_kv",
            &["count", "resolved_database", "stores"],
        ) {
            if payload["count"] != serde_json::json!(1)
                || payload["stores"] != serde_json::json!(["routed"])
            {
                failures.push(format!(
                    "kv_list_stores attached populated database: legacy store list changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_size attached populated store",
        "kv_size",
        serde_json::json!({"store": "routed", "database": "MiXeD_Kv"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_size attached populated store",
            &result,
            "mixed_kv",
            &["bytes", "resolved_database", "size", "store"],
        ) {
            if payload["size"] != serde_json::json!(4) || payload["bytes"] != serde_json::json!(11)
            {
                failures.push(format!(
                    "kv_size attached populated store: count/bytes changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_pop attached found",
        "kv_pop",
        serde_json::json!({"store": "routed", "database": "mixed_kv"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_pop attached found",
            &result,
            "mixed_kv",
            &["found", "key", "resolved_database", "value"],
        ) {
            if payload["found"] != serde_json::json!(true)
                || payload["key"] != serde_json::json!("a")
                || payload["value"] != serde_json::json!("A")
            {
                failures.push(format!(
                    "kv_pop attached found: legacy pop ordering/value changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_delete attached found",
        "kv_delete",
        serde_json::json!({"store": "routed", "key": "b", "database": "MIXED_KV"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_delete attached found",
            &result,
            "mixed_kv",
            &["deleted", "key", "resolved_database", "store"],
        ) {
            if payload["deleted"] != serde_json::json!(true)
                || payload["store"] != serde_json::json!("routed")
                || payload["key"] != serde_json::json!("b")
            {
                failures.push(format!(
                    "kv_delete attached found: legacy delete shape changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_clear attached populated store",
        "kv_clear",
        serde_json::json!({"store": "routed", "database": "mixed_kv"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_clear attached populated store",
            &result,
            "mixed_kv",
            &["removed", "resolved_database", "store"],
        ) {
            if payload["removed"] != serde_json::json!(2)
                || payload["store"] != serde_json::json!("routed")
            {
                failures.push(format!(
                    "kv_clear attached populated store: removed count changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_pop attached empty after clear",
        "kv_pop",
        serde_json::json!({"store": "routed", "database": "MiXeD_Kv"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_pop attached empty after clear",
            &result,
            "mixed_kv",
            &["found", "resolved_database"],
        ) {
            if payload["found"] != serde_json::json!(false) {
                failures.push(format!(
                    "kv_pop attached empty after clear: empty branch changed: {payload}"
                ));
            }
        }
    }

    if let Some(result) = call_case!(
        "kv_clear attached idempotent empty",
        "kv_clear",
        serde_json::json!({"store": "routed", "database": "mixed_kv"})
    ) {
        if let Some(payload) = record_kv_response(
            &mut failures,
            "kv_clear attached idempotent empty",
            &result,
            "mixed_kv",
            &["removed", "resolved_database", "store"],
        ) {
            if payload["removed"] != serde_json::json!(0) {
                failures.push(format!(
                    "kv_clear attached idempotent empty: idempotent branch changed: {payload}"
                ));
            }
        }
    }

    if let Err(error) = h.shutdown().await {
        failures.push(format!("test harness shutdown failed: {error}"));
    }
    assert!(
        failures.is_empty(),
        "resolved_database KV success-shape regressions:\n- {}",
        failures.join("\n- ")
    );
    Ok(())
}
