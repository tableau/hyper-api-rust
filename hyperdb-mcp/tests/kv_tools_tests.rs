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
