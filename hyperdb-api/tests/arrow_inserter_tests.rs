// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the `ArrowInserter` API.
//!
//! These tests cover construction and its error paths, status accessors,
//! cancellation and drop behavior, and inserting real Arrow IPC streams
//! through `insert_data` and `insert_raw`, including with a small flush
//! threshold.

use hyperdb_api::{ArrowInserter, Catalog, SqlType, TableDefinition};

mod common;
use common::TestConnection;

// =============================================================================
// ArrowInserter Construction Tests
// =============================================================================

#[test]
fn test_arrow_inserter_new() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("test_arrow")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("value", SqlType::double());

    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let inserter = ArrowInserter::new(&test.connection, &table_def);
    assert!(inserter.is_ok(), "ArrowInserter::new should succeed");

    let inserter = inserter.unwrap();
    assert!(!inserter.has_data());
    assert_eq!(inserter.total_bytes(), 0);
    assert_eq!(inserter.chunk_count(), 0);

    // Cancel to clean up
    inserter.cancel();
}

#[test]
fn test_arrow_inserter_empty_table_def() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // Empty table definition should fail
    let empty_def = TableDefinition::new("empty");
    let inserter = ArrowInserter::new(&test.connection, &empty_def);
    assert!(
        inserter.is_err(),
        "ArrowInserter::new should fail with empty table definition"
    );
}

#[test]
fn test_arrow_inserter_from_table() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // Create a table first
    test.connection
        .execute_command(
            "CREATE TABLE existing_table (
                id INTEGER NOT NULL,
                name TEXT,
                value DOUBLE PRECISION
            )",
        )
        .expect("Failed to create table");

    // Create inserter from existing table
    let inserter = ArrowInserter::from_table(&test.connection, "existing_table");
    assert!(
        inserter.is_ok(),
        "ArrowInserter::from_table should succeed for existing table"
    );

    inserter.unwrap().cancel();
}

#[test]
fn test_arrow_inserter_from_nonexistent_table() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // Try to create inserter from non-existent table
    let inserter = ArrowInserter::from_table(&test.connection, "nonexistent_table");
    assert!(
        inserter.is_err(),
        "ArrowInserter::from_table should fail for non-existent table"
    );
}

// =============================================================================
// ArrowInserter Schema Handling Tests
// =============================================================================

#[test]
fn test_arrow_inserter_insert_record_batches_without_schema_fails() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def =
        TableDefinition::new("test_arrow_schema").add_required_column("id", SqlType::int());

    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let mut inserter =
        ArrowInserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Trying to insert record batches without first sending schema should fail
    let fake_batch_data = vec![1, 2, 3, 4]; // Not real Arrow data, just for testing error
    let result = inserter.insert_record_batches(&fake_batch_data);
    assert!(
        result.is_err(),
        "insert_record_batches should fail without prior insert_data"
    );

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("No Arrow schema has been sent"),
        "Error should mention schema not sent: {err_msg}"
    );

    inserter.cancel();
}

#[test]
fn test_arrow_inserter_empty_data_is_noop() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("test_empty").add_required_column("id", SqlType::int());

    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let mut inserter =
        ArrowInserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Empty data should be a no-op (no error, no side effects)
    let result = inserter.insert_data(&[]);
    assert!(result.is_ok(), "insert_data with empty data should succeed");
    assert!(!inserter.has_data());
    assert_eq!(inserter.chunk_count(), 0);

    inserter.cancel();
}

#[test]
fn test_arrow_inserter_execute_without_data() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("test_no_data").add_required_column("id", SqlType::int());

    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let inserter =
        ArrowInserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Execute without any data should return 0 rows
    let rows = inserter.execute().expect("execute should succeed");
    assert_eq!(rows, 0, "execute without data should return 0 rows");

    // Verify table is still empty
    let count = test
        .execute_scalar_i64("SELECT COUNT(*) FROM test_no_data")
        .expect("Failed to count");
    assert_eq!(count, 0);
}

// =============================================================================
// ArrowInserter Cancel Tests
// =============================================================================

#[test]
fn test_arrow_inserter_cancel_no_data() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("test_cancel").add_required_column("id", SqlType::int());

    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let inserter =
        ArrowInserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Cancel should not panic
    inserter.cancel();

    // Connection should still be usable
    let count = test
        .execute_scalar_i64("SELECT COUNT(*) FROM test_cancel")
        .expect("Connection should still work after cancel");
    assert_eq!(count, 0);
}

#[test]
fn test_arrow_inserter_drop_without_execute() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("test_drop").add_required_column("id", SqlType::int());

    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    // Create and drop inserter without calling execute or cancel
    {
        let _inserter =
            ArrowInserter::new(&test.connection, &table_def).expect("Failed to create inserter");
        // Inserter is dropped here
    }

    // Connection should still be usable (Drop impl should clean up properly)
    let count = test
        .execute_scalar_i64("SELECT COUNT(*) FROM test_drop")
        .expect("Connection should still work after inserter drop");
    assert_eq!(count, 0);
}

// =============================================================================
// ArrowInserter IPC Insert Tests
// =============================================================================

#[test]
fn test_arrow_inserter_insert_data_ipc_stream() {
    use arrow::array::Int32Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::StreamWriter;
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("arrow_ipc").add_required_column("id", SqlType::int());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5]))],
    )
    .expect("Failed to build batch");

    let mut buffer = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut buffer, &schema).expect("stream writer");
        writer.write(&batch).expect("write batch");
        writer.finish().expect("finish stream");
    }

    let mut inserter =
        ArrowInserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    inserter.insert_data(&buffer).expect("insert_data");
    let rows = inserter.execute().expect("execute");
    assert_eq!(rows, 5);

    let count = test
        .execute_scalar_i64("SELECT COUNT(*) FROM arrow_ipc")
        .expect("count");
    assert_eq!(count, 5);
    let sum = test
        .execute_scalar_i64("SELECT SUM(id) FROM arrow_ipc")
        .expect("sum");
    assert_eq!(sum, 15);
}

/// Serializes one Arrow IPC stream with an `id: Int32` schema and one batch per entry.
fn int_ipc_stream(batches: &[Vec<i32>]) -> Vec<u8> {
    use arrow::array::Int32Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::StreamWriter;
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)]));
    let mut buffer = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut buffer, &schema).expect("stream writer");
        for values in batches {
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![Arc::new(Int32Array::from(values.clone()))],
            )
            .expect("Failed to build batch");
            writer.write(&batch).expect("write batch");
        }
        writer.finish().expect("finish stream");
    }
    buffer
}

#[test]
fn test_arrow_inserter_insert_raw_ipc_stream() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("arrow_raw").add_required_column("id", SqlType::int());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let buffer = int_ipc_stream(&[vec![1, 2, 3]]);
    let mut inserter =
        ArrowInserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    inserter.insert_raw(&buffer).expect("insert_raw");
    assert_eq!(inserter.execute().expect("execute"), 3);

    let sum = test
        .execute_scalar_i64("SELECT SUM(id) FROM arrow_raw")
        .expect("sum");
    assert_eq!(sum, 6);
}

#[test]
fn test_arrow_inserter_small_flush_threshold_multiple_batches() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("arrow_flush").add_required_column("id", SqlType::int());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let buffer = int_ipc_stream(&[vec![1, 2], vec![3, 4], vec![5, 6]]);
    let mut inserter = ArrowInserter::new(&test.connection, &table_def)
        .expect("Failed to create inserter")
        .with_flush_threshold(1);
    inserter.insert_data(&buffer).expect("insert_data");
    assert_eq!(inserter.execute().expect("execute"), 6);

    let count = test
        .execute_scalar_i64("SELECT COUNT(*) FROM arrow_flush")
        .expect("count");
    assert_eq!(count, 6);
    let sum = test
        .execute_scalar_i64("SELECT SUM(id) FROM arrow_flush")
        .expect("sum");
    assert_eq!(sum, 21);
}
