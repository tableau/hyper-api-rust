// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Round-trip serialization/deserialization tests.
//!
//! These integration tests verify that data written through the Inserter (using `HyperBinary`
//! COPY protocol) can be correctly read back through query results. This catches format
//! mismatches between our encoding and what the Hyper server expects.
//!
//! Each test:
//! 1. Creates a table with specific column types
//! 2. Inserts known values using the Inserter (`HyperBinary` encoding)
//! 3. Queries the data back (server decodes and re-encodes)
//! 4. Verifies the returned values match the originals
//!
//! This validates the complete encoding/decoding pipeline against actual Hyper server behavior.

use hyperdb_api::{Catalog, Inserter, Numeric, Row, Rowset, SqlType, TableDefinition};

mod common;
use common::TestConnection;

/// Helper to collect all rows from a streaming Rowset
fn collect_rows<T, F>(mut result: Rowset<'_>, f: F) -> Vec<T>
where
    F: Fn(&Row) -> T,
{
    let mut rows = Vec::new();
    while let Some(chunk) = result.next_chunk().expect("Failed to get chunk") {
        for row in &chunk {
            rows.push(f(row));
        }
    }
    rows
}

// =============================================================================
// Integer Types Round-Trip Tests
// =============================================================================

/// Test SMALLINT round-trip.
///
/// This test verifies that SMALLINT values round-trip correctly through insert/query.
#[test]
fn test_roundtrip_smallint() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def =
        TableDefinition::new("rt_smallint").add_nullable_column("val", SqlType::small_int());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let test_values: Vec<Option<i16>> = vec![
        Some(0),
        Some(1),
        Some(-1),
        Some(i16::MAX),
        Some(i16::MIN),
        Some(32767),
        Some(-32768),
        None, // NULL value
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    // Query back and verify
    let result = test
        .connection
        .execute_query("SELECT val FROM rt_smallint")
        .expect("Failed to query");

    let returned: Vec<Option<i16>> = collect_rows(result, |row| row.get(0));

    assert_eq!(
        returned, test_values,
        "SMALLINT round-trip mismatch: expected {test_values:?}, got {returned:?}"
    );
}

#[test]
fn test_roundtrip_integer() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("rt_integer").add_nullable_column("val", SqlType::int());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let test_values: Vec<Option<i32>> = vec![
        Some(0),
        Some(1),
        Some(-1),
        Some(i32::MAX),
        Some(i32::MIN),
        Some(2147483647),
        Some(-2147483648),
        Some(42),
        Some(-42),
        None,
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    let result = test
        .connection
        .execute_query("SELECT val FROM rt_integer")
        .expect("Failed to query");

    let returned: Vec<Option<i32>> = collect_rows(result, |row| row.get(0));

    assert_eq!(
        returned, test_values,
        "INTEGER round-trip mismatch: expected {test_values:?}, got {returned:?}"
    );
}

#[test]
fn test_roundtrip_bigint() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def =
        TableDefinition::new("rt_bigint").add_nullable_column("val", SqlType::big_int());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let test_values: Vec<Option<i64>> = vec![
        Some(0),
        Some(1),
        Some(-1),
        Some(i64::MAX),
        Some(i64::MIN),
        Some(9223372036854775807),
        Some(-9223372036854775808),
        Some(1234567890123456789),
        None,
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    let result = test
        .connection
        .execute_query("SELECT val FROM rt_bigint")
        .expect("Failed to query");

    let returned: Vec<Option<i64>> = collect_rows(result, |row| row.get(0));

    assert_eq!(
        returned, test_values,
        "BIGINT round-trip mismatch: expected {test_values:?}, got {returned:?}"
    );
}

// =============================================================================
// Floating Point Types Round-Trip Tests
// =============================================================================

#[test]
#[expect(
    clippy::approx_constant,
    reason = "test literal chosen for readability; not intended as an approximation"
)]
fn test_roundtrip_double() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("rt_double").add_nullable_column("val", SqlType::double());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    // Note: f64::MAX and f64::MIN overflow to infinity in Hyper, so we use safer values
    // Also avoid values that require more precision than f64 can reliably round-trip
    let test_values: Vec<Option<f64>> = vec![
        Some(0.0),
        Some(1.0),
        Some(-1.0),
        Some(3.14159265358979),
        Some(1e50),  // Large but not extreme
        Some(-1e50), // Large negative but not extreme
        Some(f64::MIN_POSITIVE),
        Some(1e-100),
        None,
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    let result = test
        .connection
        .execute_query("SELECT val FROM rt_double")
        .expect("Failed to query");

    let returned: Vec<Option<f64>> = collect_rows(result, |row| row.get(0));

    // Compare with tolerance for floating point
    assert_eq!(returned.len(), test_values.len());
    for (i, (expected, actual)) in test_values.iter().zip(returned.iter()).enumerate() {
        match (expected, actual) {
            (None, None) => {}
            (Some(e), Some(a)) => {
                assert!(
                    (e - a).abs() < 1e-10 || (e.is_nan() && a.is_nan()),
                    "DOUBLE round-trip mismatch at index {i}: expected {e}, got {a}"
                );
            }
            _ => panic!(
                "DOUBLE round-trip NULL mismatch at index {i}: expected {expected:?}, got {actual:?}"
            ),
        }
    }
}

/// Test for REAL (32-bit float) - SKIPPED because Hyper doesn't support it.
///
/// Hyper returns: "This database does not support 32-bit floating points. (0A000)"
/// Use DOUBLE PRECISION instead.
#[test]
#[ignore = "Hyper does not support 32-bit floating points"]
fn test_roundtrip_real() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("rt_real").add_nullable_column("val", SqlType::float());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");
}

// =============================================================================
// Boolean Round-Trip Tests
// =============================================================================

#[test]
fn test_roundtrip_boolean() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("rt_bool").add_nullable_column("val", SqlType::bool());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let test_values: Vec<Option<bool>> = vec![Some(true), Some(false), None];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    let result = test
        .connection
        .execute_query("SELECT val FROM rt_bool")
        .expect("Failed to query");

    let returned: Vec<Option<bool>> = collect_rows(result, |row| row.get(0));

    assert_eq!(
        returned, test_values,
        "BOOLEAN round-trip mismatch: expected {test_values:?}, got {returned:?}"
    );
}

// =============================================================================
// String/Text Round-Trip Tests
// =============================================================================

#[test]
fn test_roundtrip_text() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("rt_text").add_nullable_column("val", SqlType::text());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let long_string = "very long string ".repeat(100);
    let test_values: Vec<Option<&str>> = vec![
        Some(""),
        Some("hello"),
        Some("Hello, World!"),
        Some("special chars: !@#$%^&*()"),
        Some("quotes: \"double\" and 'single'"),
        Some("unicode: 你好世界 🎉 émojis"),
        Some("newlines:\nand\ttabs"),
        Some(long_string.as_str()),
        Some("null byte in middle: \0 after"),
        None,
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    let result = test
        .connection
        .execute_query("SELECT val FROM rt_text")
        .expect("Failed to query");

    let returned: Vec<Option<String>> = collect_rows(result, |row| row.get(0));

    // Convert expected to owned strings for comparison
    let expected: Vec<Option<String>> = test_values
        .iter()
        .map(|v| v.map(std::string::ToString::to_string))
        .collect();

    assert_eq!(
        returned, expected,
        "TEXT round-trip mismatch:\nexpected: {expected:?}\ngot: {returned:?}"
    );
}

#[test]
fn test_roundtrip_varchar() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // Create table with VARCHAR(100)
    test.execute_command("CREATE TABLE rt_varchar (val VARCHAR(100))")
        .expect("Failed to create table");

    let max_length_value = "x".repeat(100);
    let test_values: Vec<Option<&str>> = vec![
        Some(""),
        Some("short"),
        Some(max_length_value.as_str()),
        None,
    ];

    // Get table definition from catalog for Inserter
    let catalog = Catalog::new(&test.connection);
    let table_def = catalog
        .get_table_definition("rt_varchar")
        .expect("Failed to get table definition");

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    let result = test
        .connection
        .execute_query("SELECT val FROM rt_varchar")
        .expect("Failed to query");

    let returned: Vec<Option<String>> = collect_rows(result, |row| row.get(0));

    let expected: Vec<Option<String>> = test_values
        .iter()
        .map(|v| v.map(std::string::ToString::to_string))
        .collect();

    assert_eq!(returned, expected, "VARCHAR round-trip mismatch");
}

// =============================================================================
// Date/Time Round-Trip Tests
// =============================================================================

#[test]
fn test_roundtrip_date() {
    use hyperdb_api::Date;

    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("rt_date").add_nullable_column("val", SqlType::date());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let test_values: Vec<Option<Date>> = vec![
        Some(Date::new(2024, 1, 1)),
        Some(Date::new(2024, 12, 31)),
        Some(Date::new(1970, 1, 1)),   // Unix epoch
        Some(Date::new(2000, 2, 29)),  // Leap year
        Some(Date::new(1, 1, 1)),      // Minimum year
        Some(Date::new(9999, 12, 31)), // Maximum year
        None,
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    // Query and verify using raw string comparison, which checks the server's
    // rendering independently of the client-side decoder
    let result = test
        .connection
        .execute_query("SELECT val::TEXT FROM rt_date")
        .expect("Failed to query");

    let expected_strings: Vec<Option<String>> = vec![
        Some("2024-01-01".to_string()),
        Some("2024-12-31".to_string()),
        Some("1970-01-01".to_string()),
        Some("2000-02-29".to_string()),
        Some("0001-01-01".to_string()),
        Some("9999-12-31".to_string()),
        None,
    ];

    let returned: Vec<Option<String>> = collect_rows(result, |row| row.get(0));

    assert_eq!(returned, expected_strings, "DATE round-trip mismatch");
}

// =============================================================================
// Numeric/Decimal Round-Trip Tests
// =============================================================================

#[test]
fn test_roundtrip_numeric_small() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // NUMERIC(10, 2) - fits in i64 storage (precision <= 18)
    test.execute_command("CREATE TABLE rt_numeric_small (val NUMERIC(10, 2))")
        .expect("Failed to create table");

    let catalog = Catalog::new(&test.connection);
    let table_def = catalog
        .get_table_definition("rt_numeric_small")
        .expect("Failed to get table definition");

    let test_values: Vec<Option<Numeric>> = vec![
        Some(Numeric::new(0, 2)),             // 0.00
        Some(Numeric::new(100, 2)),           // 1.00
        Some(Numeric::new(-100, 2)),          // -1.00
        Some(Numeric::new(12345, 2)),         // 123.45
        Some(Numeric::new(-12345, 2)),        // -123.45
        Some(Numeric::new(99999999, 2)),      // 999999.99
        Some(Numeric::new(9_999_999_999, 2)), // 99999999.99 (max for precision 10)
        None,
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    // Query as text for exact comparison
    let result = test
        .connection
        .execute_query("SELECT val::TEXT FROM rt_numeric_small")
        .expect("Failed to query");

    let expected_strings: Vec<Option<String>> = vec![
        Some("0.00".to_string()),
        Some("1.00".to_string()),
        Some("-1.00".to_string()),
        Some("123.45".to_string()),
        Some("-123.45".to_string()),
        Some("999999.99".to_string()),
        Some("99999999.99".to_string()),
        None,
    ];

    let returned: Vec<Option<String>> = collect_rows(result, |row| row.get(0));

    assert_eq!(
        returned, expected_strings,
        "NUMERIC(10,2) round-trip mismatch"
    );
}

/// A `Numeric` whose scale differs from the column's must be rescaled to the
/// column's scale, not written as a raw unscaled value.
/// Regression: `add_numeric` ignored the value's scale, so `1234.5` inserted
/// into `NUMERIC(10, 2)` was stored as `12.35`/`123.45` (magnitude corruption).
#[test]
fn test_roundtrip_numeric_scale_mismatch() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // One small (i64) and one big (128-bit) storage column.
    test.execute_command(
        "CREATE TABLE rt_numeric_rescale (small NUMERIC(10, 2), big NUMERIC(30, 4))",
    )
    .expect("Failed to create table");
    let table_def = Catalog::new(&test.connection)
        .get_table_definition("rt_numeric_rescale")
        .expect("Failed to get table definition");

    let rows: Vec<(Numeric, Numeric)> = vec![
        (Numeric::new(12345, 1), Numeric::new(12345, 1)), // 1234.5 -> scale up
        (Numeric::new(5, 0), Numeric::new(5, 0)),         // 5 -> scale up
        (Numeric::new(12_300, 4), Numeric::new(1_234_500, 6)), // trailing zeros -> scale down, exact
        (Numeric::new(-25, 1), Numeric::new(-25, 1)),          // -2.5
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for (small, big) in &rows {
        inserter
            .add_row(&[&Some(*small), &Some(*big)])
            .expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    let result = test
        .connection
        .execute_query("SELECT small::TEXT, big::TEXT FROM rt_numeric_rescale")
        .expect("Failed to query");
    let returned: Vec<(Option<String>, Option<String>)> =
        collect_rows(result, |row| (row.get(0), row.get(1)));
    let want = [
        ("1234.50", "1234.5000"),
        ("5.00", "5.0000"),
        ("1.23", "1.2345"),
        ("-2.50", "-2.5000"),
    ];
    for ((got_s, got_b), (want_s, want_b)) in returned.iter().zip(want) {
        assert_eq!(got_s.as_deref(), Some(want_s));
        assert_eq!(got_b.as_deref(), Some(want_b));
    }
    assert_eq!(returned.len(), want.len());
}

/// A `Numeric` that cannot be represented at the column's scale without
/// losing digits must be rejected, not silently truncated.
#[test]
fn test_roundtrip_numeric_scale_mismatch_lossy_rejected() {
    let test = TestConnection::new().expect("Failed to create test connection");
    test.execute_command("CREATE TABLE rt_numeric_lossy (v NUMERIC(10, 2))")
        .expect("Failed to create table");
    let table_def = Catalog::new(&test.connection)
        .get_table_definition("rt_numeric_lossy")
        .expect("Failed to get table definition");

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    let err = inserter
        .add_row(&[&Some(Numeric::new(12_345, 3))]) // 12.345 has 3 decimals, column holds 2
        .expect_err("lossy rescale must be rejected");
    assert!(
        err.to_string().contains("scale"),
        "error should explain the scale problem, got: {err}"
    );
}

// =============================================================================
// Binary Data Round-Trip Tests
// =============================================================================

/// Test BYTEA round-trip.
///
/// This test verifies that BYTEA values round-trip correctly through insert/query.
/// BYTEA is returned as raw bytes in `HyperBinary`; `get::<Vec<u8>>` yields them unchanged.
#[test]
fn test_roundtrip_bytea() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("rt_bytea").add_nullable_column("val", SqlType::bytes());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let test_values: Vec<Option<Vec<u8>>> = vec![
        Some(vec![]),                       // Empty
        Some(vec![0x00]),                   // Single null byte
        Some(vec![0xFF]),                   // Single max byte
        Some(vec![0x00, 0x01, 0x02, 0x03]), // Sequential
        Some(vec![0xDE, 0xAD, 0xBE, 0xEF]), // Common test pattern
        Some((0..=255).collect()),          // All possible byte values
        Some(vec![0x00; 1000]),             // Many null bytes
        None,
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for val in &test_values {
        inserter.add_row(&[val]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    let result = test
        .connection
        .execute_query("SELECT val FROM rt_bytea")
        .expect("Failed to query");

    let returned: Vec<Option<Vec<u8>>> = collect_rows(result, |row| row.get(0));

    assert_eq!(returned, test_values, "BYTEA round-trip mismatch");
}

// =============================================================================
// Multi-Column Round-Trip Tests
// =============================================================================

/// Test mixed types round-trip.
///
/// This test verifies that multiple column types can be inserted and queried together.
#[test]
fn test_roundtrip_mixed_types() {
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command(
        "CREATE TABLE rt_mixed (
            id INT NOT NULL,
            name TEXT,
            amount DOUBLE PRECISION,
            active BOOL,
            code SMALLINT
        )",
    )
    .expect("Failed to create table");

    let catalog = Catalog::new(&test.connection);
    let table_def = catalog
        .get_table_definition("rt_mixed")
        .expect("Failed to get table definition");

    // Test data with various combinations
    // Note: f64::MAX overflows to infinity in Hyper, so we use a large but finite value
    #[expect(
        clippy::type_complexity,
        reason = "bespoke tuple of scalar sample rows for a single-use test vector"
    )]
    let test_rows: Vec<(i32, Option<&str>, Option<f64>, Option<bool>, Option<i16>)> = vec![
        (1, Some("Alice"), Some(100.50), Some(true), Some(1)),
        (2, Some("Bob"), Some(-50.25), Some(false), Some(-1)),
        (3, None, None, None, None), // All NULLs except ID
        (4, Some(""), Some(0.0), Some(false), Some(0)), // Edge values
        (
            5,
            Some("Unicode: 日本語"),
            Some(1e50),
            Some(true),
            Some(i16::MAX),
        ),
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for (id, name, amount, active, code) in &test_rows {
        inserter
            .add_row(&[id, name, amount, active, code])
            .expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    // Verify
    let result = test
        .connection
        .execute_query("SELECT id, name, amount, active, code FROM rt_mixed ORDER BY id")
        .expect("Failed to query");

    #[expect(
        clippy::type_complexity,
        reason = "bespoke tuple of scalar sample rows for a single-use test vector"
    )]
    let returned: Vec<(i32, Option<String>, Option<f64>, Option<bool>, Option<i16>)> =
        collect_rows(result, |row| {
            (
                row.get::<i32>(0).unwrap(),
                row.get::<String>(1),
                row.get::<f64>(2),
                row.get::<bool>(3),
                row.get::<i16>(4),
            )
        });

    assert_eq!(returned.len(), test_rows.len());

    for (i, ((id, name, amount, active, code), ret)) in
        test_rows.iter().zip(returned.iter()).enumerate()
    {
        assert_eq!(*id, ret.0, "ID mismatch at row {i}");
        assert_eq!(
            name.map(std::string::ToString::to_string),
            ret.1,
            "Name mismatch at row {i}"
        );
        match (amount, &ret.2) {
            (None, None) => {}
            (Some(e), Some(a)) => {
                assert!(
                    (*e - *a).abs() < 1e-10,
                    "Amount mismatch at row {i}: {e} vs {a}"
                );
            }
            _ => panic!("Amount NULL mismatch at row {i}"),
        }
        assert_eq!(*active, ret.3, "Active mismatch at row {i}");
        assert_eq!(*code, ret.4, "Code mismatch at row {i}");
    }
}

// =============================================================================
// Edge Cases and Stress Tests
// =============================================================================

#[test]
fn test_roundtrip_large_batch() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let table_def = TableDefinition::new("rt_large_batch")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("data", SqlType::text());
    Catalog::new(&test.connection)
        .create_table(&table_def)
        .expect("Failed to create table");

    let row_count = 10_000;

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for i in 0..row_count {
        let data = if i % 10 == 0 {
            None
        } else {
            Some(format!("row_{i}"))
        };
        inserter.add_row(&[&i, &data]).expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    // Verify count
    let count: i64 = test
        .connection
        .execute_scalar_query("SELECT COUNT(*) FROM rt_large_batch")
        .expect("Failed to count")
        .expect("NULL count");

    assert_eq!(count, i64::from(row_count), "Row count mismatch");

    // Verify a few specific rows
    let result = test
        .connection
        .execute_query("SELECT id, data FROM rt_large_batch WHERE id IN (0, 1, 9999) ORDER BY id")
        .expect("Failed to query");

    let rows: Vec<(i32, Option<String>)> = collect_rows(result, |row| {
        (row.get::<i32>(0).unwrap(), row.get::<String>(1))
    });

    assert_eq!(rows[0], (0, None)); // Every 10th row is NULL
    assert_eq!(rows[1], (1, Some("row_1".to_string())));
    assert_eq!(rows[2], (9999, Some("row_9999".to_string())));
}

/// Test boundary values for all integer types.
///
/// This test verifies that MIN/MAX boundary values for integers round-trip correctly.
#[test]
fn test_roundtrip_boundary_values() {
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command(
        "CREATE TABLE rt_boundary (
            i16_val SMALLINT,
            i32_val INT,
            i64_val BIGINT,
            f64_val DOUBLE PRECISION
        )",
    )
    .expect("Failed to create table");

    let catalog = Catalog::new(&test.connection);
    let table_def = catalog
        .get_table_definition("rt_boundary")
        .expect("Failed to get table definition");

    // Test boundary values
    // Note: f64::MAX/MIN overflow to infinity in Hyper, so we use large but finite values
    #[expect(
        clippy::type_complexity,
        reason = "bespoke tuple of scalar sample rows for a single-use test vector"
    )]
    let test_data: Vec<(Option<i16>, Option<i32>, Option<i64>, Option<f64>)> = vec![
        (Some(i16::MIN), Some(i32::MIN), Some(i64::MIN), Some(-1e50)),
        (Some(i16::MAX), Some(i32::MAX), Some(i64::MAX), Some(1e50)),
        (Some(0), Some(0), Some(0), Some(0.0)),
        (Some(-1), Some(-1), Some(-1), Some(-1.0)),
        (Some(1), Some(1), Some(1), Some(1.0)),
    ];

    let mut inserter =
        Inserter::new(&test.connection, &table_def).expect("Failed to create inserter");
    for (i16_val, i32_val, i64_val, f64_val) in &test_data {
        inserter
            .add_row(&[i16_val, i32_val, i64_val, f64_val])
            .expect("Failed to add row");
    }
    inserter.execute().expect("Failed to execute");

    let result = test
        .connection
        .execute_query("SELECT * FROM rt_boundary")
        .expect("Failed to query");

    #[expect(
        clippy::type_complexity,
        reason = "bespoke tuple of scalar sample rows for a single-use test vector"
    )]
    let returned: Vec<(Option<i16>, Option<i32>, Option<i64>, Option<f64>)> =
        collect_rows(result, |row| {
            (
                row.get::<i16>(0),
                row.get::<i32>(1),
                row.get::<i64>(2),
                row.get::<f64>(3),
            )
        });

    for (i, (expected, actual)) in test_data.iter().zip(returned.iter()).enumerate() {
        assert_eq!(expected.0, actual.0, "i16 mismatch at row {i}");
        assert_eq!(expected.1, actual.1, "i32 mismatch at row {i}");
        assert_eq!(expected.2, actual.2, "i64 mismatch at row {i}");
        // Special handling for f64 comparison
        match (expected.3, actual.3) {
            (Some(e), Some(a)) => {
                assert!((e - a).abs() < 1e-10, "f64 mismatch at row {i}: {e} vs {a}");
            }
            (None, None) => {}
            _ => panic!("f64 NULL mismatch at row {i}"),
        }
    }
}
