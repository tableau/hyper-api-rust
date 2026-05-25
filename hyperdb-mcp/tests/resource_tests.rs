// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for MCP Resources exposed by the server: URI list, URI parsing,
//! and content for workspace / tables / per-table schema resources.

use hyperdb_mcp::server::HyperMcpServer;
use tempfile::TempDir;

/// Build a server with a fresh temp workspace, populate the engine's
/// ephemeral primary with a test table, and return both the server and
/// the temp dir.
///
/// The server is constructed without a persistent path so the MCP-managed
/// `_table_catalog` doesn't appear alongside `widgets` and perturb the
/// exact table counts these tests assert against. Catalog behavior is
/// covered in `tests/table_catalog_tests.rs`.
///
/// Uses `no_daemon` mode to avoid interference from any daemon running
/// in parallel (e.g. from daemon_tests in the same `cargo test` run).
///
/// To seed the table inside the server's own engine (so the data is
/// visible to resource handlers), we read `hyper://workspace` first to
/// trigger lazy engine init, then reach into the engine handle to run
/// the seeding DDL/DML directly.
fn server_with_test_table() -> (HyperMcpServer, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("workspace.hyper");
    let server = HyperMcpServer::with_no_daemon(Some(path.to_str().unwrap().into()), false, true);
    // Trigger lazy engine init.
    let _ = server.resource_body_for_uri("hyper://workspace");
    // Seed the test table in the server's ephemeral primary so the
    // resource handlers see it through `describe_tables` etc.
    {
        let handle = server.engine_handle();
        let guard = handle.lock().expect("engine mutex");
        let engine = guard.as_ref().expect("engine initialized");
        engine
            .execute_command("CREATE TABLE widgets (id INT NOT NULL, name TEXT)")
            .unwrap();
        engine
            .execute_command("INSERT INTO widgets VALUES (1, 'Alpha')")
            .unwrap();
        engine
            .execute_command("INSERT INTO widgets VALUES (2, 'Beta')")
            .unwrap();
    }
    (server, dir)
}

/// Verify that `list_resource_uris` includes the workspace, tables list,
/// readme, and three per-table URIs (schema + sample + csv-sample).
#[test]
fn list_resources_includes_workspace_tables_readme_and_per_table() {
    let (server, _dir) = server_with_test_table();
    let uris = server.list_resource_uris();
    assert!(uris.contains(&"hyper://workspace".to_string()));
    assert!(uris.contains(&"hyper://tables".to_string()));
    assert!(uris.contains(&"hyper://readme".to_string()));
    assert!(uris.contains(&"hyper://tables/widgets/schema".to_string()));
    assert!(uris.contains(&"hyper://tables/widgets/sample".to_string()));
    assert!(uris.contains(&"hyper://tables/widgets/csv-sample".to_string()));
}

/// Verify that reading <hyper://workspace> returns the workspace status JSON
/// including the `hyper_rust_api_version` field (`.r<hash>`-suffixed).
#[test]
fn read_workspace_resource_returns_status() {
    let (server, _dir) = server_with_test_table();
    let body = server
        .resource_body_for_uri("hyper://workspace")
        .unwrap()
        .expect("workspace resource should exist");
    assert_eq!(body.mime_type(), "application/json");
    let json = body.as_json().expect("workspace is JSON");
    assert_eq!(json["hyperd_running"], true);
    assert!(json["ephemeral_path"].is_string());
    // `persistent_path` is either a string (when persistent attached) or
    // null (--ephemeral-only). The test fixture supplies a path so we
    // expect the string form; the orthogonal `has_persistent` flag mirrors
    // the same fact and is checked first.
    assert_eq!(json["has_persistent"], true);
    assert!(json["persistent_path"].is_string());
    assert_eq!(json["table_count"], 1);
    let version = json["hyper_rust_api_version"]
        .as_str()
        .expect("workspace resource carries hyper_rust_api_version");
    assert!(version.contains(".r"));
}

/// Verify that reading <hyper://tables> returns the tables list with schemas
/// and row counts.
#[test]
fn read_tables_resource_returns_all_tables() {
    let (server, _dir) = server_with_test_table();
    let body = server
        .resource_body_for_uri("hyper://tables")
        .unwrap()
        .expect("tables resource should exist");
    let json = body.as_json().expect("tables is JSON");
    let tables = json["tables"].as_array().unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0]["name"], "widgets");
    assert_eq!(tables[0]["row_count"], 2);
}

/// Verify that reading <hyper://tables/{name}/schema> returns just that table's
/// schema information.
#[test]
fn read_table_schema_resource_returns_single_table() {
    let (server, _dir) = server_with_test_table();
    let body = server
        .resource_body_for_uri("hyper://tables/widgets/schema")
        .unwrap()
        .expect("widgets schema resource should exist");
    let json = body.as_json().expect("schema is JSON");
    assert_eq!(json["name"], "widgets");
    assert_eq!(json["row_count"], 2);
    let cols = json["columns"].as_array().unwrap();
    assert_eq!(cols.len(), 2);
}

/// Reading <hyper://tables/{name}/sample> returns the first rows as a JSON
/// object with both `schema` and `rows` — the shape that `sample_table`
/// returns, so LLMs see column types alongside values.
#[test]
fn read_table_sample_resource_returns_rows_and_schema() {
    let (server, _dir) = server_with_test_table();
    let body = server
        .resource_body_for_uri("hyper://tables/widgets/sample")
        .unwrap()
        .expect("widgets sample resource should exist");
    assert_eq!(body.mime_type(), "application/json");
    let json = body.as_json().expect("sample is JSON");
    assert_eq!(json["table"], "widgets");
    assert_eq!(json["row_count"], 2);
    let rows = json["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "fixture has 2 rows, sample returns both");
    let schema = json["schema"].as_array().unwrap();
    assert!(
        !schema.is_empty(),
        "schema should be populated from catalog"
    );
}

/// Reading <hyper://tables/{name}/csv-sample> returns `text/csv` with a header
/// row and one data row per fixture entry, in declared column order.
#[test]
fn read_table_csv_sample_resource_emits_csv() {
    let (server, _dir) = server_with_test_table();
    let body = server
        .resource_body_for_uri("hyper://tables/widgets/csv-sample")
        .unwrap()
        .expect("widgets csv-sample resource should exist");
    assert_eq!(body.mime_type(), "text/csv");
    let text = body.to_text();
    let mut lines = text.lines();
    let header = lines.next().expect("CSV has a header row");
    assert_eq!(header, "id,name");
    let data: Vec<&str> = lines.collect();
    assert_eq!(data.len(), 2);
    assert!(data[0].starts_with("1,") && data[0].contains("Alpha"));
    assert!(data[1].starts_with("2,") && data[1].contains("Beta"));
}

/// Reading <hyper://readme> returns markdown listing every table, its row
/// count, and pointers to the per-table resources.
#[test]
fn read_readme_resource_lists_tables_in_markdown() {
    let (server, _dir) = server_with_test_table();
    let body = server
        .resource_body_for_uri("hyper://readme")
        .unwrap()
        .expect("readme resource should exist");
    assert_eq!(body.mime_type(), "text/markdown");
    let text = body.to_text();
    assert!(text.starts_with("# HyperDB workspace"));
    assert!(text.contains("`widgets`"));
    assert!(text.contains("hyper://tables/widgets/schema"));
    assert!(text.contains("hyper://tables/widgets/sample"));
    assert!(text.contains("hyper://tables/widgets/csv-sample"));
    assert!(text.contains("## Tool hints"));
}

/// An empty workspace (no tables) still produces a valid readme that hints
/// at `load_file` / `inspect_file` so a cold-started LLM knows what to do
/// first.
#[test]
fn read_readme_resource_handles_empty_workspace() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.hyper");
    // No persistent path so `_table_catalog` doesn't make the workspace non-empty.
    let server = HyperMcpServer::with_no_daemon(Some(path.to_str().unwrap().into()), false, true);
    let body = server
        .resource_body_for_uri("hyper://readme")
        .unwrap()
        .expect("readme resource should exist even with no tables");
    let text = body.to_text();
    assert!(text.contains("No tables loaded yet"));
    assert!(text.contains("load_file"));
    assert!(text.contains("inspect_file"));
}

/// Verify that an unknown URI returns Ok(None) so the async trait can map it
/// to an `invalid_params` error.
#[test]
fn unknown_uri_returns_none() {
    let (server, _dir) = server_with_test_table();
    let result = server.resource_body_for_uri("hyper://bogus").unwrap();
    assert!(result.is_none());
}

/// Verify that requesting a missing table's schema returns a `TableNotFound` error
/// rather than None (it's a syntactically valid URI, just wrong table).
#[test]
fn missing_table_schema_returns_error() {
    let (server, _dir) = server_with_test_table();
    let result = server.resource_body_for_uri("hyper://tables/ghost/schema");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code, hyperdb_mcp::error::ErrorCode::TableNotFound);
}

/// A sample URI for a missing table surfaces the same `TableNotFound` error as
/// the schema URI (catalog lookup fails identically for both).
#[test]
fn missing_table_sample_returns_error() {
    let (server, _dir) = server_with_test_table();
    let result = server.resource_body_for_uri("hyper://tables/ghost/sample");
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().code,
        hyperdb_mcp::error::ErrorCode::TableNotFound
    );
}
