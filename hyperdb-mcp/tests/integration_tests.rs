// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! End-to-end integration tests that exercise the full pipeline:
//! ingest → query → export, similar to how the MCP tools compose.
//! These tests verify that multiple modules work together correctly.

mod common;
use common::TestEngine;
use hyperdb_mcp::export::{export_to_file, ExportOptions};
use hyperdb_mcp::ingest::{ingest_csv, ingest_json, IngestOptions};

/// Load two related JSON datasets (orders + customers), then run a JOIN with
/// GROUP BY aggregation. Verifies multi-table workspace queries work end-to-end.
#[test]
fn full_pipeline_json_to_query() {
    let te = TestEngine::new_ephemeral();

    let orders = r#"[
        {"order_id": 1, "customer_id": 1, "amount": 100.50},
        {"order_id": 2, "customer_id": 2, "amount": 200.00},
        {"order_id": 3, "customer_id": 1, "amount": 50.25}
    ]"#;
    let opts = IngestOptions {
        table: "orders".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, orders, &opts).unwrap();

    let customers = r#"[
        {"customer_id": 1, "name": "Alice"},
        {"customer_id": 2, "name": "Bob"}
    ]"#;
    let opts = IngestOptions {
        table: "customers".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, customers, &opts).unwrap();

    let rows = te.engine.execute_query_to_json(
        "SELECT c.name, SUM(o.amount) as total FROM orders o JOIN customers c ON o.customer_id = c.customer_id GROUP BY c.name ORDER BY total DESC"
    ).unwrap();

    assert_eq!(rows.len(), 2);
}

/// Ingest CSV, run a computed-column query (quantity * price), and export
/// the result to a CSV file. Verifies the full ingest → transform → export
/// pipeline that the `query_file` MCP tool relies on.
#[test]
fn full_pipeline_csv_ingest_and_export() {
    let te = TestEngine::new_ephemeral();

    let csv_data = "product,quantity,price\nWidget,100,9.99\nGadget,50,19.99\n";
    let opts = IngestOptions {
        table: "products".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_csv(&te.engine, csv_data, &opts).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let export_path = dir.path().join("export.csv");
    let export_path_str = export_path.to_str().unwrap();
    let export_opts = ExportOptions {
        sql: Some(
            "SELECT product, quantity * price as revenue FROM products ORDER BY revenue DESC"
                .into(),
        ),
        table: None,
        path: export_path_str.into(),
        format: "csv".into(),
        overwrite: true,
        format_options: None,
    };
    let result = export_to_file(&te.engine, &export_opts).unwrap();
    assert_eq!(result.rows, 2);

    let contents = std::fs::read_to_string(export_path_str).unwrap();
    assert!(contents.contains("Gadget"));
}

/// Create multiple tables via DDL and verify `describe_tables` returns all of
/// them with accurate row counts (including zero-row tables).
#[test]
fn describe_shows_all_tables() {
    let te = TestEngine::new_ephemeral();

    te.engine
        .execute_command("CREATE TABLE t1 (a INT)")
        .unwrap();
    te.engine
        .execute_command("CREATE TABLE t2 (b TEXT)")
        .unwrap();
    te.engine
        .execute_command("INSERT INTO t1 VALUES (1)")
        .unwrap();

    let tables = te.engine.describe_tables().unwrap();
    assert_eq!(tables.len(), 2);

    let t1 = tables.iter().find(|t| t["name"] == "t1").unwrap();
    assert_eq!(t1["row_count"], 1);
}

/// Verify that `status()` reflects the current workspace state after creating
/// a table and inserting a row — `table_count`, `total_rows`, and hyperd health.
#[test]
fn status_reports_workspace_info() {
    let te = TestEngine::new_ephemeral();
    te.engine
        .execute_command("CREATE TABLE metrics (id INT)")
        .unwrap();
    te.engine
        .execute_command("INSERT INTO metrics VALUES (1)")
        .unwrap();

    let status = te.engine.status().unwrap();
    assert_eq!(status["hyperd_running"], true);
    assert_eq!(status["table_count"], 1);
    assert_eq!(status["total_rows"], 1);
}

/// Verify that append mode across multiple ingest calls accumulates rows
/// rather than replacing them. First call loads 2 rows, second appends 1,
/// final count should be 3.
#[test]
fn append_mode_accumulates_data() {
    let te = TestEngine::new_ephemeral();

    let batch1 = r#"[{"v": 1}, {"v": 2}]"#;
    let batch2 = r#"[{"v": 3}]"#;

    let opts_replace = IngestOptions {
        table: "acc".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let opts_append = IngestOptions {
        table: "acc".into(),
        mode: "append".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };

    ingest_json(&te.engine, batch1, &opts_replace).unwrap();
    ingest_json(&te.engine, batch2, &opts_append).unwrap();

    let rows = te
        .engine
        .execute_query_to_json("SELECT COUNT(*) as cnt FROM acc")
        .unwrap();
    assert_eq!(rows[0]["cnt"], 3);
}
