// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the Inserter API.

use hyperdb_api::{Catalog, Date, Geography, Inserter, SqlType, TableDefinition};

mod common;
use common::TestConnection;

#[test]
fn test_inserter_null_in_non_null_column() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("foo").add_required_column("a", SqlType::int());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Trying to insert NULL into a NOT NULL column should fail early during add_row
    let result = inserter.add_row(&[&None::<i32>]);
    assert!(
        result.is_err(),
        "add_row should fail for NULL in NOT NULL column"
    );
}

#[test]
fn test_inserter_type_match() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("foo").add_nullable_column("a", SqlType::int());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Insert an integer value (matching the column type)
    inserter.add_row(&[&42i32]).expect("add_row should succeed");
    inserter.execute().expect("execute should succeed");

    // Verify the value was inserted correctly
    let mut result = test
        .connection
        .execute_query("SELECT a FROM foo")
        .expect("Failed to query");
    let chunk = result
        .next_chunk()
        .expect("Failed to get chunk")
        .expect("Expected chunk");
    let value = chunk
        .first()
        .expect("Expected row")
        .get_i32(0)
        .expect("NULL value");
    assert_eq!(value, 42);
}

/// Tests that inserter creation succeeds for non-existent tables, but execution fails.
///
/// # Deferred Validation Design
///
/// The Inserter uses **deferred validation** by design:
///
/// 1. **`Inserter::new()`** succeeds even if the target table doesn't exist. This is because
///    the inserter only buffers data locally; no database communication occurs until `execute()`.
///
/// 2. **`add_row()`** succeeds because it only validates row structure against the `TableDefinition`,
///    not against the actual database schema.
///
/// 3. **`execute()`** is where actual database communication happens (COPY protocol), so this
///    is when table existence and schema mismatches are detected.
///
/// This design allows for:
/// - Batch preparation before database round-trips
/// - Better performance by avoiding early validation queries
/// - Simpler error handling (all DB errors occur at execute time)
///
/// The tradeoff is that errors like missing tables are detected later, which may be
/// counterintuitive but follows the principle of lazy evaluation for I/O operations.
#[test]
fn test_inserter_missing_table() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("nonexistent").add_nullable_column("a", SqlType::int());

    // Inserter creation succeeds - no database communication yet (deferred validation)
    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Inserter creation should succeed");

    // add_row succeeds - only local buffering, no DB validation
    inserter.add_row(&[&42i32]).expect("add_row should succeed");

    // execute() fails - this is when COPY protocol actually communicates with the database
    // and discovers the table doesn't exist
    assert!(
        inserter.execute().is_err(),
        "execute should fail for non-existent table"
    );
}

#[test]
#[expect(
    clippy::approx_constant,
    reason = "test literal 3.14 chosen for readability; not intended as an approximation of PI"
)]
fn test_inserter_all_types() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // Create a simple table with a subset of types to test basic functionality
    let table_def = TableDefinition::new("all_types_test")
        .add_nullable_column("col_bool", SqlType::bool())
        .add_nullable_column("col_int", SqlType::int())
        .add_nullable_column("col_bigint", SqlType::big_int())
        .add_nullable_column("col_text", SqlType::text())
        .add_nullable_column("col_double", SqlType::double())
        .add_nullable_column("col_date", SqlType::date());

    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Insert a row with all types
    let date = Date::new(2024, 1, 15);
    inserter
        .add_row(&[
            &true,
            &42i32,
            &1234567890123i64,
            &"hello",
            &3.14159f64,
            &date,
        ])
        .expect("Failed to add row");

    inserter.execute().expect("Failed to execute inserter");

    // Verify the data
    let mut result = test
        .connection
        .execute_query("SELECT * FROM all_types_test")
        .expect("Failed to query");

    let chunk = result
        .next_chunk()
        .expect("Failed to get chunk")
        .expect("Expected chunk");
    let row = chunk.first().expect("Expected row");

    let bool_val = row.get::<bool>(0).expect("NULL bool");
    let int_val = row.get_i32(1).expect("NULL int");
    let bigint_val = row.get_i64(2).expect("NULL bigint");
    let text_val = row.get::<String>(3).expect("NULL text");
    let double_val = row.get_f64(4).expect("NULL double");

    assert!(bool_val);
    assert_eq!(int_val, 42);
    assert_eq!(bigint_val, 1234567890123i64);
    assert_eq!(text_val, "hello");
    assert!((double_val - 3.14159).abs() < 0.0001);
}

#[test]
fn test_inserter_bulk_insert() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("numbers")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("value", SqlType::double());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Insert many rows
    for i in 0..1000 {
        inserter
            .add_row(&[&i, &(f64::from(i) * 1.5)])
            .expect("Failed to add row");
    }

    inserter.execute().expect("Failed to execute inserter");

    // Verify all rows were inserted
    let count = test
        .count_tuples("numbers")
        .expect("Failed to count tuples");
    assert_eq!(count, 1000);
}

#[test]
fn test_inserter_nullable_columns() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("nullable_test")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("name", SqlType::text());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Insert row with value
    inserter
        .add_row(&[&1i32, &Some("Alice")])
        .expect("Failed to add row with value");

    // Insert row with NULL
    inserter
        .add_row(&[&2i32, &None::<&str>])
        .expect("Failed to add row with NULL");

    inserter.execute().expect("Failed to execute inserter");

    // Verify rows
    let mut result = test
        .connection
        .execute_query("SELECT id, name FROM nullable_test ORDER BY id")
        .expect("Failed to query table");

    let mut rows: Vec<(i32, Option<String>)> = Vec::new();
    while let Some(chunk) = result.next_chunk().expect("Failed to get chunk") {
        for row in &chunk {
            let id = row.get_i32(0).expect("NULL id");
            let name = row.get::<String>(1);
            rows.push((id, name));
        }
    }

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], (1, Some("Alice".to_string())));
    assert_eq!(rows[1], (2, None));
}

#[test]
fn test_inserter_from_table() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // Create table using SQL
    test.execute_command(
        "CREATE TABLE products (id INT NOT NULL, name TEXT, price DOUBLE PRECISION)",
    )
    .expect("Failed to create table");

    // Get the table definition from catalog
    let catalog = Catalog::new(&test.connection);
    let table_def = catalog
        .get_table_definition("products")
        .expect("Failed to get table definition");

    // Create inserter from the table definition
    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    inserter
        .add_row(&[&1i32, &"Widget", &9.99f64])
        .expect("Failed to add row");
    inserter
        .add_row(&[&2i32, &"Gadget", &19.99f64])
        .expect("Failed to add row");

    inserter.execute().expect("Failed to execute inserter");

    // Verify
    let count = test.count_tuples("products").expect("Failed to count");
    assert_eq!(count, 2);
}

#[test]
fn test_inserter_geography_round_trip() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("geo_test")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("location", SqlType::geography());

    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Create a Geography from WKT (San Francisco coordinates)
    let geo = Geography::from_wkt("POINT(-122.4194 37.7749)")
        .expect("Failed to create geography from WKT");
    let original_bytes = geo.as_bytes().to_vec();

    // Insert using IntoValue trait
    inserter.add_row(&[&1i32, &geo]).expect("Failed to add row");

    inserter.execute().expect("Failed to execute inserter");

    // Read back and verify byte-identical
    let mut result = test
        .connection
        .execute_query("SELECT id, location FROM geo_test")
        .expect("Failed to query");

    let chunk = result
        .next_chunk()
        .expect("Failed to get chunk")
        .expect("Expected chunk");
    let row = chunk.first().expect("Expected row");

    let id = row.get_i32(0).expect("NULL id");
    assert_eq!(id, 1);

    // Geography does NOT implement RowValue yet, so read as Vec<u8>
    let geo_bytes = row.get::<Vec<u8>>(1).expect("NULL geography");
    assert_eq!(
        geo_bytes, original_bytes,
        "Geography bytes should be byte-identical after round-trip"
    );
}

#[test]
fn test_inserter_geography_nullable() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("geo_nullable_test")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("location", SqlType::geography());

    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");

    // Insert a row with a Geography value
    let geo1 = Geography::from_wkt("POINT(-122.4194 37.7749)")
        .expect("Failed to create geography from WKT");
    inserter
        .add_row(&[&1i32, &Some(geo1)])
        .expect("Failed to add row with geography");

    // Insert a row with NULL
    inserter
        .add_row(&[&2i32, &None::<Geography>])
        .expect("Failed to add row with NULL");

    inserter.execute().expect("Failed to execute inserter");

    // Verify rows
    let mut result = test
        .connection
        .execute_query("SELECT id, location FROM geo_nullable_test ORDER BY id")
        .expect("Failed to query table");

    let mut rows: Vec<(i32, Option<Vec<u8>>)> = Vec::new();
    while let Some(chunk) = result.next_chunk().expect("Failed to get chunk") {
        for row in &chunk {
            let id = row.get_i32(0).expect("NULL id");
            let location = row.get::<Vec<u8>>(1);
            rows.push((id, location));
        }
    }

    assert_eq!(rows.len(), 2);
    assert!(rows[0].1.is_some(), "First row should have a geography");
    assert!(rows[1].1.is_none(), "Second row should have NULL");
}
