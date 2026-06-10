// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for ToSqlParam implementations.
//!
//! These tests verify that types implementing ToSqlParam correctly encode
//! parameters for use with query_params(), validating against actual Hyper behavior.

use hyperdb_api::{Interval, Numeric, ToSqlParam};

mod common;
use common::TestConnection;

/// Test JSON parameter round-trip.
#[test]
fn test_json_param() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let json_value = serde_json::json!({"a": 1, "b": [2, 3]});
    let result = test
        .connection
        .query_params("SELECT $1 AS v", &[&json_value as &dyn ToSqlParam])
        .expect("query_params failed");

    let rows = result.collect_rows().expect("collect_rows failed");
    assert_eq!(rows.len(), 1, "Expected exactly one row");

    // Read back as String and verify it parses to the same JSON value
    let returned_str: Option<String> = rows[0].get(0);
    assert!(returned_str.is_some(), "Expected non-NULL JSON value");

    let returned_json: serde_json::Value =
        serde_json::from_str(&returned_str.unwrap()).expect("Failed to parse returned JSON");
    assert_eq!(
        returned_json, json_value,
        "Returned JSON doesn't match original"
    );
}

/// Test Interval parameter — verifies the binary encoding decodes to the
/// correct value server-side by rendering the bound param as text.
#[test]
fn test_interval_param() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let interval = Interval::new(2, 5, 0); // 2 months, 5 days, 0 microseconds
                                           // CAST the bound interval param to text so we can assert the VALUE, not
                                           // just non-null — this proves the [us BE][days BE][months BE] encoding
                                           // was interpreted correctly (Interval doesn't yet implement RowValue, so
                                           // we can't read it back as a typed Interval).
    let result = test
        .connection
        .query_params(
            "SELECT CAST($1 AS text) AS v",
            &[&interval as &dyn ToSqlParam],
        )
        .expect("query_params failed");

    let rows = result.collect_rows().expect("collect_rows failed");
    assert_eq!(rows.len(), 1, "Expected exactly one row");

    let returned: String = rows[0].get(0).expect("Expected non-NULL Interval value");
    // Hyper renders intervals in ISO-8601 duration form: "P2M5D" (Period,
    // 2 Months, 5 Days). Asserting the exact rendering proves the
    // [us BE][days BE][months BE] field encoding decoded correctly — a
    // swapped or mis-scaled field would produce a different string.
    assert_eq!(
        returned, "P2M5D",
        "interval should decode to 2 months + 5 days (ISO-8601), got: {returned}"
    );
}

/// Test Option<Numeric> (nullable param via the blanket Option impl).
#[test]
fn test_option_numeric_param() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // Some(scale=0) binds the value; None binds SQL NULL.
    let some_n: Option<Numeric> = Some(Numeric::new(7, 0));
    let none_n: Option<Numeric> = None;

    let rows = test
        .connection
        .query_params(
            "SELECT $1 AS a, $2 AS b",
            &[&some_n as &dyn ToSqlParam, &none_n as &dyn ToSqlParam],
        )
        .expect("query_params failed")
        .collect_rows()
        .expect("collect_rows failed");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<i64>(0), Some(7), "Some(Numeric(7,0)) → 7");
    assert_eq!(rows[0].get::<i64>(1), None, "None → SQL NULL");
}

/// Test Numeric scale=0 parameter round-trip.
#[test]
fn test_numeric_scale0_param() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let numeric = Numeric::new(42, 0); // scale = 0
    let result = test
        .connection
        .query_params("SELECT $1 AS v", &[&numeric as &dyn ToSqlParam])
        .expect("query_params failed");

    let rows = result.collect_rows().expect("collect_rows failed");
    assert_eq!(rows.len(), 1, "Expected exactly one row");

    // Read back as i64 and verify
    let returned: Option<i64> = rows[0].get(0);
    assert_eq!(
        returned,
        Some(42),
        "Expected Numeric(42,0) to round-trip as 42"
    );
}

/// Pins the documented scale>0 limitation: Hyper rejects binary NUMERIC
/// params that carry a non-zero dscale.
///
/// We encode the TRUE scale (dscale = 2 for `Numeric::new(123, 2)` == 1.23),
/// so the value is represented faithfully on the wire — and Hyper then
/// rejects it server-side with SQLSTATE `0A000` ("cannot handle truncation
/// when reading numerics") rather than silently truncating it to a
/// mis-scaled integer. This is fail-fast, not silent corruption.
///
/// The error surfaces at `collect_rows()` time (when the Bind/Execute round
/// trip completes), NOT at `query_params()` time. When scaled support lands
/// (#132), this test flips to a success assertion.
#[test]
fn test_numeric_scaled_rejected_fail_fast() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let numeric = Numeric::new(123, 2); // 1.23 — scale > 0
    let result = test
        .connection
        .query_params("SELECT $1 AS v", &[&numeric as &dyn ToSqlParam])
        .expect("query_params itself should not error");

    let err = result
        .collect_rows()
        .expect_err("scale>0 Numeric must be rejected by the server, not silently truncated");
    let msg = err.to_string();
    assert!(
        msg.contains("0A000") || msg.contains("cannot handle truncation when reading numerics"),
        "expected Hyper's NUMERIC truncation error (fail-fast), got: {msg}"
    );
}
