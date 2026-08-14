// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compatibility and size contracts for the generated MCP tool catalog.
//!
//! The harness lists tools through the real in-memory rmcp client/server
//! boundary. `HyperMcpServer` remains un-warmed, so catalog inspection never
//! starts Hyper or opens a database.

use hyperdb_mcp::readme::README;
use hyperdb_mcp::server::HyperMcpServer;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientInfo, Tool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::{ClientHandler, ServiceExt};
use serde::Serialize;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

const CATALOG_BYTE_BUDGET: usize = 57_344;

const LEGACY_TOOL_NAMES: [&str; 33] = [
    "attach_database",
    "chart",
    "copy_query",
    "delete_query",
    "describe",
    "detach_database",
    "execute",
    "export",
    "get_readme",
    "inspect_file",
    "kv_clear",
    "kv_delete",
    "kv_get",
    "kv_list",
    "kv_list_stores",
    "kv_pop",
    "kv_set",
    "kv_set_many",
    "kv_size",
    "list_attached_databases",
    "load_data",
    "load_file",
    "load_files",
    "load_iceberg",
    "query",
    "query_data",
    "query_file",
    "sample",
    "save_query",
    "set_table_metadata",
    "status",
    "unwatch_directory",
    "watch_directory",
];

#[derive(Debug, Clone)]
struct DummyClientHandler;

impl ClientHandler for DummyClientHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

struct CatalogHarness {
    client: RunningService<RoleClient, DummyClientHandler>,
    server_handle: tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
}

impl CatalogHarness {
    async fn start(read_only: bool) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (server_io, client_io) = tokio::io::duplex(128 * 1024);
        let server = HyperMcpServer::with_no_daemon(None, read_only, true);

        let server_handle =
            tokio::spawn(async move {
                let running = server.serve(server_io).await.map_err(
                    |error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) },
                )?;
                running.waiting().await.map_err(
                    |error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) },
                )?;
                Ok(())
            });

        let client = DummyClientHandler
            .serve(client_io)
            .await
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;

        Ok(Self {
            client,
            server_handle,
        })
    }

    async fn list_all_tools(&self) -> Result<Vec<Tool>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .list_all_tools()
            .await
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })
    }

    fn initialization_instructions(&self) -> Result<String, std::io::Error> {
        self.client
            .peer_info()
            .and_then(|info| info.instructions.clone())
            .ok_or_else(|| std::io::Error::other("server returned no initialization instructions"))
    }

    async fn get_readme(&self) -> Result<CallToolResult, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .call_tool(CallToolRequestParams::new("get_readme"))
            .await
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })
    }

    async fn shutdown(self) -> TestResult {
        self.client
            .cancel()
            .await
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
        self.server_handle.await??;
        Ok(())
    }
}

fn sorted_names(tools: &[Tool]) -> Vec<&str> {
    let mut names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    names
}

fn sorted_tools(mut tools: Vec<Tool>) -> Vec<Tool> {
    tools.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    tools
}

fn serialized_len<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_vec(value)
        .expect("catalog values must serialize")
        .len()
}

fn readme_text(result: &CallToolResult) -> Result<&str, std::io::Error> {
    let mut text_blocks = result
        .content
        .iter()
        .filter_map(|content| content.raw.as_text());
    let text = text_blocks
        .next()
        .ok_or_else(|| std::io::Error::other("get_readme returned no text content"))?;
    if text_blocks.next().is_some() {
        return Err(std::io::Error::other(
            "get_readme returned more than one text content block",
        ));
    }
    Ok(&text.text)
}

#[tokio::test]
async fn generated_catalog_preserves_full_33_tool_contract() -> TestResult {
    let writable_harness = CatalogHarness::start(false).await?;
    let writable_tools = writable_harness.list_all_tools().await?;
    writable_harness.shutdown().await?;

    assert_eq!(
        sorted_names(&writable_tools),
        LEGACY_TOOL_NAMES,
        "the generated writable catalog must preserve the exact legacy surface"
    );
    assert!(
        writable_tools.iter().all(|tool| tool.name != "doctor"),
        "doctor is a native CLI subcommand, not an MCP tool"
    );

    let read_only_harness = CatalogHarness::start(true).await?;
    let read_only_tools = read_only_harness.list_all_tools().await?;
    read_only_harness.shutdown().await?;

    assert_eq!(
        sorted_names(&read_only_tools),
        LEGACY_TOOL_NAMES,
        "read-only mode must advertise the same complete legacy surface"
    );
    assert_eq!(
        sorted_tools(read_only_tools),
        sorted_tools(writable_tools),
        "read-only mode must preserve every generated tool field"
    );
    Ok(())
}

#[tokio::test]
async fn generated_catalog_budget_and_metadata_contract() -> TestResult {
    let harness = CatalogHarness::start(false).await?;
    let tools = harness.list_all_tools().await?;
    let instructions = harness.initialization_instructions()?;
    let get_readme_result = harness.get_readme().await?;
    harness.shutdown().await?;

    assert_eq!(tools.len(), LEGACY_TOOL_NAMES.len());

    let canonical_payload = serde_json::to_vec(&tools)?;
    let mut total_tool_bytes = 0;
    let mut total_name_bytes = 0;
    let mut total_description_bytes = 0;
    let mut total_input_schema_bytes = 0;
    let mut tool_metrics = Vec::with_capacity(tools.len());

    for tool in &tools {
        assert!(
            tool.output_schema.is_none(),
            "legacy tool `{}` unexpectedly gained an output schema",
            tool.name
        );
        assert!(
            tool.annotations.is_none(),
            "legacy tool `{}` unexpectedly gained annotations",
            tool.name
        );

        let tool_bytes = serialized_len(tool);
        // Human-readable strings are charged by their UTF-8 content bytes;
        // JSON quoting/escaping and field punctuation belong to `other`.
        // Object-valued schemas retain their canonical minified JSON size.
        let name_bytes = tool.name.len();
        let description_bytes = tool.description.as_deref().map_or(0, str::len);
        let input_schema_bytes = serialized_len(tool.input_schema.as_ref());
        let other_bytes = tool_bytes
            .checked_sub(name_bytes + description_bytes + input_schema_bytes)
            .expect("catalog byte categories must not exceed their tool object");

        total_tool_bytes += tool_bytes;
        total_name_bytes += name_bytes;
        total_description_bytes += description_bytes;
        total_input_schema_bytes += input_schema_bytes;
        tool_metrics.push((
            tool.name.as_ref(),
            tool_bytes,
            name_bytes,
            description_bytes,
            input_schema_bytes,
            other_bytes,
        ));
    }

    let vec_framing_bytes = if tools.is_empty() { 2 } else { tools.len() + 1 };
    assert_eq!(
        canonical_payload.len(),
        total_tool_bytes + vec_framing_bytes,
        "canonical Vec<Tool> bytes must equal tool objects plus array punctuation"
    );
    let total_other_bytes = canonical_payload
        .len()
        .checked_sub(total_name_bytes + total_description_bytes + total_input_schema_bytes)
        .expect("catalog byte categories must not exceed the canonical payload");

    tool_metrics.sort_unstable_by_key(|metrics| metrics.0);
    for (name, total, name_bytes, description, input_schema, other) in tool_metrics {
        println!(
            "catalog_tool name={name} total_bytes={total} name_bytes={name_bytes} \
             description_bytes={description} input_schema_bytes={input_schema} \
             other_bytes={other} output_schema=absent annotations=absent"
        );
    }

    let get_readme_text = readme_text(&get_readme_result)?;
    assert_eq!(
        get_readme_text, README,
        "the generated get_readme tool must return the canonical README"
    );
    println!(
        "catalog_total tools={} total_bytes={} name_bytes={} description_bytes={} \
         input_schema_bytes={} other_bytes={} budget_bytes={}",
        tools.len(),
        canonical_payload.len(),
        total_name_bytes,
        total_description_bytes,
        total_input_schema_bytes,
        total_other_bytes,
        CATALOG_BYTE_BUDGET
    );
    println!(
        "initialization_instructions_utf8_bytes={}",
        instructions.len()
    );
    println!("get_readme_utf8_bytes={}", get_readme_text.len());

    assert!(
        canonical_payload.len() <= CATALOG_BYTE_BUDGET,
        "canonical generated catalog is {} bytes, exceeding the reviewed {}-byte budget",
        canonical_payload.len(),
        CATALOG_BYTE_BUDGET
    );
    Ok(())
}

#[tokio::test]
async fn generated_catalog_readme_coverage_contract() -> TestResult {
    let harness = CatalogHarness::start(false).await?;
    let tools = harness.list_all_tools().await?;
    let get_readme_result = harness.get_readme().await?;
    harness.shutdown().await?;

    let generated_readme = readme_text(&get_readme_result)?;
    assert_eq!(generated_readme, README);

    let undocumented: Vec<_> = sorted_names(&tools)
        .into_iter()
        .filter(|name| !generated_readme.contains(&format!("`{name}`")))
        .collect();
    assert!(
        undocumented.is_empty(),
        "generated tools missing an exact backticked README mention: {undocumented:?}"
    );
    Ok(())
}
