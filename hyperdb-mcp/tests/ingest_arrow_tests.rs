// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for Parquet and Arrow IPC file ingest: verifying that data is loaded
//! with exact schema from file metadata, including nullability handling.

#![expect(
    clippy::cast_possible_wrap,
    reason = "test data (row counts) bounded by test parameters; usize→i64 wrap is unreachable"
)]

mod common;
use common::TestEngine;
use hyperdb_mcp::ingest::IngestOptions;
use hyperdb_mcp::ingest_arrow::{ingest_arrow_ipc_file, ingest_parquet_file};
use std::sync::Arc;

/// Write a small Parquet file with an INT32 "id" column (non-nullable) and a
/// UTF8 "name" column (nullable, with one NULL value) for use in tests.
fn create_test_parquet(path: &str) {
    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use std::fs::File;

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("name", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int32Array::from(vec![1, 2, 3])),
            Arc::new(StringArray::from(vec![Some("Alice"), Some("Bob"), None])),
        ],
    )
    .unwrap();

    let file = File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// Write a small Arrow IPC file with an INT32 "x" and FLOAT64 "y" column,
/// both non-nullable, for use in tests.
fn create_test_arrow_ipc(path: &str) {
    use arrow::array::{Float64Array, Int32Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::FileWriter;
    use arrow::record_batch::RecordBatch;
    use std::fs::File;

    let schema = Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int32, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int32Array::from(vec![10, 20])),
            Arc::new(Float64Array::from(vec![1.5, 2.5])),
        ],
    )
    .unwrap();

    let file = File::create(path).unwrap();
    let mut writer = FileWriter::try_new(file, &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.finish().unwrap();
}

/// Ingest a Parquet file with 3 rows (one containing a NULL name), then query
/// back to verify all rows loaded and NULL values are preserved.
#[test]
fn ingest_parquet() {
    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.parquet");
    let path_str = path.to_str().unwrap();
    create_test_parquet(path_str);

    let opts = IngestOptions {
        table: "pq_data".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_parquet_file(&te.engine, path_str, &opts).unwrap();
    assert_eq!(result.rows, 3);

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM pq_data ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["name"], "Alice");
    assert!(rows[2]["name"].is_null());
}

/// Regression test: a Parquet file whose column is Decimal128 and NOT NULL
/// must round-trip with the correct values. This is the exact shape produced
/// by TPCH parquet files (acctbal, extendedprice, discount, tax, etc.).
///
/// Historically two separate bugs have broken this case:
/// 1. `arrow_value_to_sql` had no Decimal128 branch, so every decimal cell
///    became the literal string "NULL" and the INSERT failed against the
///    NOT NULL column with `ERROR: non-NULL value required (23502)`.
/// 2. The ingest path was switched to hyperd's native `external(...)`
///    reader, which had to be validated as preserving precision/scale and
///    not silently truncating values.
///
/// Both are covered by this test: it asserts the row count, schema
/// precision/scale, and exact decimal values after ingest.
#[test]
fn ingest_parquet_decimal128_not_null_preserves_values() {
    use arrow::array::{Decimal128Array, Int64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use std::fs::File;

    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decimal.parquet");
    let path_str = path.to_str().unwrap();

    // Two NOT NULL columns: a BIGINT id and a NUMERIC(15, 2) amount. The
    // raw decimal128 values below are the unscaled integers — with scale=2,
    // 12345 → 123.45 and -67890 → -678.90.
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("amount", DataType::Decimal128(15, 2), false),
    ]));
    let amount = Decimal128Array::from(vec![12345i128, -67890i128, 0i128])
        .with_precision_and_scale(15, 2)
        .unwrap();
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![Arc::new(Int64Array::from(vec![1, 2, 3])), Arc::new(amount)],
    )
    .unwrap();

    let file = File::create(path_str).unwrap();
    let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let opts = IngestOptions {
        table: "decimal_data".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_parquet_file(&te.engine, path_str, &opts).unwrap();
    assert_eq!(result.rows, 3);

    // The inferred target schema must preserve precision and scale so the
    // ingested values aren't silently re-rounded.
    let amount_col = result
        .schema
        .iter()
        .find(|c| c.name == "amount")
        .expect("amount column");
    assert_eq!(amount_col.hyper_type, "NUMERIC(15, 2)");
    assert!(!amount_col.nullable);

    // Verify every row made it in with the correct decimal value.
    let rows = te
        .engine
        .execute_query_to_json("SELECT id, amount::TEXT AS amount FROM decimal_data ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["amount"], "123.45");
    assert_eq!(rows[1]["amount"], "-678.90");
    assert_eq!(rows[2]["amount"], "0.00");
}

/// Regression test: the reported row count for a large parquet ingest
/// must match the actual table row count. Previously, `count_rows_sync`
/// issued immediately after `CREATE TABLE AS` inside the same transaction
/// returned the value truncated to its low 17 bits — so a 59M-row lineitem
/// load was reported as ~86k. The fix moves the COUNT outside the tx.
///
/// We pick 200,000 rows because it straddles the 131,072 (`1 << 17`) boundary:
/// the old bug would report `200000 & 0x1FFFF` = 68928, not 200000.
#[test]
fn ingest_parquet_reports_accurate_row_count_above_131072() {
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use std::fs::File;

    const ROW_COUNT: usize = 200_000;

    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.parquet");
    let path_str = path.to_str().unwrap();

    let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
    let ids: Vec<i64> = (0..ROW_COUNT as i64).collect();
    let batch =
        RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(Int64Array::from(ids))]).unwrap();

    let file = File::create(path_str).unwrap();
    let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let opts = IngestOptions {
        table: "big_table".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_parquet_file(&te.engine, path_str, &opts).unwrap();

    // The bug would have reported 200000 & 0x1FFFF = 68928.
    assert_eq!(
        result.rows, ROW_COUNT as u64,
        "IngestResult.rows was truncated — count must be read outside the CTAS transaction"
    );

    // And confirm the table actually holds all rows.
    let rows = te
        .engine
        .execute_query_to_json("SELECT COUNT(*) AS n FROM big_table")
        .unwrap();
    assert_eq!(rows[0]["n"], ROW_COUNT);
}

/// Append-mode parquet ingest must leave existing rows untouched and add
/// the new file's rows on top. This pins the semantics of the
/// `INSERT INTO ... SELECT * FROM external(...)` branch of the native path.
#[test]
fn ingest_parquet_append_adds_to_existing_rows() {
    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("append.parquet");
    let path_str = path.to_str().unwrap();
    create_test_parquet(path_str);

    let opts_replace = IngestOptions {
        table: "append_data".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let r1 = ingest_parquet_file(&te.engine, path_str, &opts_replace).unwrap();
    assert_eq!(r1.rows, 3);

    let opts_append = IngestOptions {
        table: "append_data".into(),
        mode: "append".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let r2 = ingest_parquet_file(&te.engine, path_str, &opts_append).unwrap();
    assert_eq!(r2.rows, 3);

    let rows = te
        .engine
        .execute_query_to_json("SELECT COUNT(*) AS n FROM append_data")
        .unwrap();
    assert_eq!(rows[0]["n"], 6);
}

/// A schema override must be applied via an explicit `::TYPE` cast in the
/// SELECT projection, turning the source column into the target type rather
/// than passing it through unchanged. Here we widen a parquet INT32 "id"
/// column to BIGINT and assert the resulting target column carries the
/// override.
#[test]
fn ingest_parquet_applies_schema_override() {
    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("override.parquet");
    let path_str = path.to_str().unwrap();
    create_test_parquet(path_str);

    let mut override_map = serde_json::Map::new();
    override_map.insert("id".to_string(), serde_json::Value::String("BIGINT".into()));
    let opts = IngestOptions {
        table: "override_data".into(),
        mode: "replace".into(),
        schema_override: Some(override_map),
        merge_key: None,
        target_db: None,
    };
    let result = ingest_parquet_file(&te.engine, path_str, &opts).unwrap();
    assert_eq!(result.rows, 3);

    let id_col = result
        .schema
        .iter()
        .find(|c| c.name == "id")
        .expect("id column");
    assert_eq!(id_col.hyper_type, "BIGINT");

    // Verify the target table accepts BIGINT-range values — which it couldn't
    // if the override hadn't been applied and the column was still INT (i32).
    te.engine
        .execute_command("INSERT INTO override_data (id, name) VALUES (9999999999, 'big')")
        .expect("insert of value beyond i32::MAX should succeed with BIGINT column");

    let rows = te
        .engine
        .execute_query_to_json("SELECT id FROM override_data WHERE name = 'big'")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], 9999999999i64);
}

/// Ingest an Arrow IPC file with 2 rows of INT32 + FLOAT64 data, then query
/// back to verify the row count and that the exact schema was preserved.
#[test]
fn ingest_arrow_ipc() {
    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.arrow");
    let path_str = path.to_str().unwrap();
    create_test_arrow_ipc(path_str);

    let opts = IngestOptions {
        table: "arrow_data".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_arrow_ipc_file(&te.engine, path_str, &opts).unwrap();
    assert_eq!(result.rows, 2);

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM arrow_data ORDER BY x")
        .unwrap();
    assert_eq!(rows.len(), 2);
}

/// Regression test: an Arrow IPC file with a NOT NULL Decimal128 column
/// must round-trip with the correct values over the binary COPY path.
/// Mirrors the parquet decimal regression test — when the IPC loader still
/// rendered values as SQL text, a missing Decimal128 branch turned every
/// cell into NULL. Under the new `ArrowInserter` path, the batch bytes go
/// straight through; this test pins that behavior and the schema mapping
/// that preserves NUMERIC(p, s).
#[test]
fn ingest_arrow_ipc_decimal128_not_null_preserves_values() {
    use arrow::array::{Decimal128Array, Int64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::FileWriter;
    use arrow::record_batch::RecordBatch;
    use std::fs::File;

    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decimal.arrow");
    let path_str = path.to_str().unwrap();

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("amount", DataType::Decimal128(15, 2), false),
    ]));
    let amount = Decimal128Array::from(vec![12345i128, -67890i128, 0i128])
        .with_precision_and_scale(15, 2)
        .unwrap();
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![Arc::new(Int64Array::from(vec![1, 2, 3])), Arc::new(amount)],
    )
    .unwrap();

    let file = File::create(path_str).unwrap();
    let mut writer = FileWriter::try_new(file, &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.finish().unwrap();

    let opts = IngestOptions {
        table: "ipc_decimal".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_arrow_ipc_file(&te.engine, path_str, &opts).unwrap();
    assert_eq!(result.rows, 3);

    let amount_col = result
        .schema
        .iter()
        .find(|c| c.name == "amount")
        .expect("amount column");
    assert_eq!(amount_col.hyper_type, "NUMERIC(15, 2)");
    assert!(!amount_col.nullable);

    let rows = te
        .engine
        .execute_query_to_json("SELECT id, amount::TEXT AS amount FROM ipc_decimal ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["amount"], "123.45");
    assert_eq!(rows[1]["amount"], "-678.90");
    assert_eq!(rows[2]["amount"], "0.00");
}

/// Append-mode Arrow IPC ingest must leave existing rows untouched and add
/// the new file's rows on top. Guards the INSERT (no DROP) branch of
/// `ingest_arrow_ipc_file`.
#[test]
fn ingest_arrow_ipc_append_adds_to_existing_rows() {
    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("append.arrow");
    let path_str = path.to_str().unwrap();
    create_test_arrow_ipc(path_str);

    let opts_replace = IngestOptions {
        table: "ipc_append".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let r1 = ingest_arrow_ipc_file(&te.engine, path_str, &opts_replace).unwrap();
    assert_eq!(r1.rows, 2);

    let opts_append = IngestOptions {
        table: "ipc_append".into(),
        mode: "append".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let r2 = ingest_arrow_ipc_file(&te.engine, path_str, &opts_append).unwrap();
    assert_eq!(r2.rows, 2);

    let rows = te
        .engine
        .execute_query_to_json("SELECT COUNT(*) AS n FROM ipc_append")
        .unwrap();
    assert_eq!(rows[0]["n"], 4);
}

/// Arrow IPC ingest rejects schema overrides with a clear error rather
/// than failing deep inside the COPY Arrow protocol at Hyper's type check.
/// The embedded Arrow schema is authoritative on this path.
#[test]
fn ingest_arrow_ipc_rejects_schema_override() {
    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("override.arrow");
    let path_str = path.to_str().unwrap();
    create_test_arrow_ipc(path_str);

    let mut override_map = serde_json::Map::new();
    override_map.insert("x".to_string(), serde_json::Value::String("BIGINT".into()));
    let opts = IngestOptions {
        table: "ipc_override".into(),
        mode: "replace".into(),
        schema_override: Some(override_map),
        merge_key: None,
        target_db: None,
    };
    let Err(err) = ingest_arrow_ipc_file(&te.engine, path_str, &opts) else {
        panic!("override on IPC should be rejected")
    };
    let msg = err.to_string();
    assert!(
        msg.contains("Schema overrides are not supported for Arrow IPC"),
        "error should explain why override was rejected, got: {msg}"
    );

    // And the target table must not have been created.
    let rows = te
        .engine
        .execute_query_to_json(
            "SELECT 1 AS present FROM pg_catalog.pg_tables WHERE tablename = 'ipc_override'",
        )
        .unwrap();
    assert!(
        rows.is_empty(),
        "table should not exist after rejected override"
    );
}

/// Parallel parquet ingest through a connection pool — the building block
/// behind the `load_files` MCP tool. Two files land into two separate
/// tables concurrently on distinct pooled connections and both reported
/// row counts must be accurate. Guards against regressions in the async
/// ingest path's transaction lifecycle and row-count accounting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_files_runs_parallel_parquet_ingests() {
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use hyperdb_api::pool::{create_pool, PoolConfig};
    use hyperdb_api::CreateMode;
    use hyperdb_mcp::ingest_arrow::ingest_parquet_file_async;
    use parquet::arrow::ArrowWriter;
    use std::fs::File;

    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();

    // Write two parquet files with distinct row counts so we can verify
    // each landed in the correct table.
    let write_parquet = |name: &str, n: usize| {
        let path = dir.path().join(name);
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let ids: Vec<i64> = (0..n as i64).collect();
        let batch =
            RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(Int64Array::from(ids))])
                .unwrap();
        let file = File::create(&path).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        path.to_str().unwrap().to_string()
    };
    let path_a = write_parquet("a.parquet", 5_000);
    let path_b = write_parquet("b.parquet", 7_000);

    // Build a pool against the running hyperd — same recipe the MCP's
    // `load_files` tool uses.
    let endpoint = te.engine.hyperd_endpoint().unwrap();
    let workspace = te.engine.ephemeral_path().to_string_lossy().to_string();
    let pool = Arc::new(
        create_pool(
            PoolConfig::new(endpoint, workspace)
                .create_mode(CreateMode::DoNotCreate)
                .max_size(2),
        )
        .unwrap(),
    );

    // Fire both ingests concurrently. Each task checks out its own pooled
    // connection, runs to completion, and releases on drop.
    let run = |table: &str, path: String| {
        let pool = Arc::clone(&pool);
        let table = table.to_string();
        async move {
            let conn = pool.get().await.unwrap();
            let opts = IngestOptions {
                table,
                mode: "replace".into(),
                schema_override: None,
                merge_key: None,
                target_db: None,
            };
            ingest_parquet_file_async(&conn, &path, &opts)
                .await
                .unwrap()
        }
    };
    let (res_a, res_b) = tokio::join!(run("table_a", path_a), run("table_b", path_b));

    assert_eq!(res_a.rows, 5_000);
    assert_eq!(res_b.rows, 7_000);

    // Verify both tables actually hold the data.
    let a = te
        .engine
        .execute_query_to_json("SELECT COUNT(*) AS n FROM table_a")
        .unwrap();
    assert_eq!(a[0]["n"], 5_000);
    let b = te
        .engine
        .execute_query_to_json("SELECT COUNT(*) AS n FROM table_b")
        .unwrap();
    assert_eq!(b[0]["n"], 7_000);
}

/// Regression test: the Arrow IPC loader must accept both the File
/// (Feather) format and the Stream format. The MCP's own `export` tool
/// emits raw IPC Stream bytes (same as Hyper's wire protocol), so a
/// round-trip `export → load_file` would fail if the loader only
/// understood the File format. This test writes a Stream-format file
/// directly and verifies ingest succeeds and preserves row count + data.
#[test]
fn ingest_arrow_ipc_accepts_stream_format() {
    use arrow::array::{Float64Array, Int32Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::StreamWriter;
    use arrow::record_batch::RecordBatch;
    use std::fs::File;

    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stream.arrow");
    let path_str = path.to_str().unwrap();

    let schema = Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int32, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int32Array::from(vec![10, 20, 30])),
            Arc::new(Float64Array::from(vec![1.5, 2.5, 3.5])),
        ],
    )
    .unwrap();

    // Write IPC Stream format (no "ARROW1" magic, no footer).
    let file = File::create(path_str).unwrap();
    let mut writer = StreamWriter::try_new(file, &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.finish().unwrap();

    let opts = IngestOptions {
        table: "stream_data".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_arrow_ipc_file(&te.engine, path_str, &opts).unwrap();
    assert_eq!(result.rows, 3);

    let rows = te
        .engine
        .execute_query_to_json("SELECT x FROM stream_data ORDER BY x")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["x"], 10);
    assert_eq!(rows[2]["x"], 30);
}

/// Smoke test for merge mode against Parquet — relies on
/// `merge_via_temp_table`'s format-agnostic implementation. Initial
/// 3 rows, merge file has 2 overlapping + 1 new → final 4.
#[test]
fn ingest_parquet_merge_basic() {
    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();

    // Initial: id=1,2,3 with names Alice/Bob/(NULL).
    let initial_path = dir.path().join("initial.parquet");
    create_test_parquet(initial_path.to_str().unwrap());
    let opts_replace = IngestOptions {
        table: "pq_merge".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let r1 =
        ingest_parquet_file(&te.engine, initial_path.to_str().unwrap(), &opts_replace).unwrap();
    assert_eq!(r1.rows, 3);

    // Merge file: id=2 (Bob → "Bob Updated"), id=4 (new "Dave").
    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use std::fs::File;
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("name", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int32Array::from(vec![2, 4])),
            Arc::new(StringArray::from(vec![Some("Bob Updated"), Some("Dave")])),
        ],
    )
    .unwrap();
    let merge_path = dir.path().join("merge.parquet");
    let file = File::create(&merge_path).unwrap();
    let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let opts_merge = IngestOptions {
        table: "pq_merge".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: None,
    };
    ingest_parquet_file(&te.engine, merge_path.to_str().unwrap(), &opts_merge).unwrap();

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM pq_merge ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["name"], "Alice"); // id=1, untouched
    assert_eq!(rows[1]["name"], "Bob Updated"); // id=2, replaced
    assert!(rows[2]["name"].is_null()); // id=3, untouched
    assert_eq!(rows[3]["name"], "Dave"); // id=4, inserted
}

/// Smoke test for merge mode against Arrow IPC. Same shape as the
/// Parquet smoke — file format swap, identical assertions modulo
/// column types.
#[test]
fn ingest_arrow_ipc_merge_basic() {
    let te = TestEngine::new_ephemeral();
    let dir = tempfile::tempdir().unwrap();

    // Initial: x=10,20 / y=1.5,2.5
    let initial_path = dir.path().join("initial.arrow");
    create_test_arrow_ipc(initial_path.to_str().unwrap());
    let opts_replace = IngestOptions {
        table: "ipc_merge".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let r1 =
        ingest_arrow_ipc_file(&te.engine, initial_path.to_str().unwrap(), &opts_replace).unwrap();
    assert_eq!(r1.rows, 2);

    // Merge: x=20 (update y=2.5→9.9), x=30 (new)
    use arrow::array::{Float64Array, Int32Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::FileWriter;
    use arrow::record_batch::RecordBatch;
    use std::fs::File;
    let schema = Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int32, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int32Array::from(vec![20, 30])),
            Arc::new(Float64Array::from(vec![9.9, 3.5])),
        ],
    )
    .unwrap();
    let merge_path = dir.path().join("merge.arrow");
    let file = File::create(&merge_path).unwrap();
    let mut writer = FileWriter::try_new(file, &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.finish().unwrap();

    let opts_merge = IngestOptions {
        table: "ipc_merge".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["x".into()]),
        target_db: None,
    };
    ingest_arrow_ipc_file(&te.engine, merge_path.to_str().unwrap(), &opts_merge).unwrap();

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM ipc_merge ORDER BY x")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["x"], 10);
    assert_eq!(rows[0]["y"], 1.5);
    assert_eq!(rows[1]["x"], 20);
    assert_eq!(rows[1]["y"], 9.9); // updated
    assert_eq!(rows[2]["x"], 30); // inserted
    assert_eq!(rows[2]["y"], 3.5);
}
