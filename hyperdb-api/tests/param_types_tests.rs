// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for ToSqlParam implementations.
//!
//! These tests verify that types implementing ToSqlParam correctly encode
//! parameters for use with query_params(), validating against actual Hyper behavior.

use hyperdb_api::{
    AsyncConnection, CreateMode, Geography, HyperProcess, Interval, Numeric, Oid, Result,
    ToSqlParam,
};
use hyperdb_api_core::types::oids;

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
    // was interpreted correctly.
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

/// Scaled `Numeric` (scale > 0) params round-trip — issue #132.
///
/// Before per-parameter format codes existed these were rejected with
/// SQLSTATE `0A000` ("cannot handle truncation when reading numerics"),
/// because every parameter went out as PG binary and Hyper has no binary
/// input path for a scaled NUMERIC. They now bind as text.
///
/// `CAST($1 AS NUMERIC(p,s))` is what pins the result type: a scaled
/// `Numeric` binds with an unspecified OID (see `ToSqlParam for Numeric`),
/// so a bare `SELECT $1` would come back as `TEXT`.
#[test]
fn test_numeric_scaled_param_round_trip() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // (unscaled, scale, precision, rendered) — scales 0, 2, 4 and 10, both signs.
    let cases: &[(i128, u8, u8, &str)] = &[
        (42, 0, 10, "42"),
        (-42, 0, 10, "-42"),
        (123_456, 2, 10, "1234.56"),
        (-123_456, 2, 10, "-1234.56"),
        (123_456_700, 4, 18, "12345.6700"),
        (-987_654_321, 4, 18, "-98765.4321"),
        (1, 10, 20, "0.0000000001"),
        (-1, 10, 20, "-0.0000000001"),
        (0, 2, 10, "0.00"),
    ];

    for &(unscaled, scale, precision, rendered) in cases {
        let numeric = Numeric::new(unscaled, scale);
        assert_eq!(
            numeric.to_string(),
            rendered,
            "test-vector sanity: Numeric({unscaled}, {scale})"
        );

        let rows = test
            .connection
            .query_params(
                &format!("SELECT CAST($1 AS NUMERIC({precision},{scale})) AS v"),
                &[&numeric as &dyn ToSqlParam],
            )
            .expect("query_params failed")
            .collect_rows()
            .unwrap_or_else(|e| panic!("scaled Numeric {rendered} must bind, got: {e}"));

        assert_eq!(rows.len(), 1, "Expected exactly one row for {rendered}");
        let returned: Numeric = rows[0]
            .get::<Numeric>(0)
            .unwrap_or_else(|| panic!("expected non-NULL NUMERIC for {rendered}"));
        assert_eq!(
            returned.to_string(),
            rendered,
            "Numeric({unscaled}, {scale}) should round-trip unchanged"
        );
        assert_eq!(returned.scale(), scale, "scale must survive the round trip");
    }
}

/// A scaled `Numeric` param against a real `NUMERIC(10,2)` column — both as
/// the INSERT value and as an equality predicate.
///
/// This is the case that actually needs the unspecified OID: the server
/// infers the parameter's type (and therefore its scale) from the column.
#[test]
fn test_numeric_scaled_param_against_column() {
    let test = TestConnection::new().expect("Failed to create test connection");
    test.connection
        .execute_command("CREATE TABLE prices (id INT, amount NUMERIC(10,2))")
        .expect("CREATE TABLE failed");

    let amount = Numeric::new(123_456, 2); // 1234.56
    let inserted = test
        .connection
        .command_params(
            "INSERT INTO prices VALUES (1, $1)",
            &[&amount as &dyn ToSqlParam],
        )
        .expect("scaled Numeric must bind as an INSERT value");
    assert_eq!(inserted, 1, "one row inserted");

    let rows = test
        .connection
        .execute_query("SELECT amount FROM prices")
        .expect("query failed")
        .collect_rows()
        .expect("collect_rows failed");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<Numeric>(0).expect("non-NULL").to_string(),
        "1234.56",
        "stored value must match what was bound"
    );

    // ...and the same value as a predicate must match the stored row.
    let matched = test
        .connection
        .query_params(
            "SELECT id FROM prices WHERE amount = $1",
            &[&amount as &dyn ToSqlParam],
        )
        .expect("query_params failed")
        .collect_rows()
        .expect("scaled Numeric must bind in a WHERE clause");
    assert_eq!(matched.len(), 1, "predicate should match the inserted row");
    assert_eq!(matched[0].get::<i32>(0), Some(1));
}

/// A text-format param and binary-format params in the *same* Bind.
///
/// Hyper accepts a mixed per-parameter format-code array, which is what
/// lets the binary fast path survive alongside text-only types. If the
/// array were ever collapsed back to a single uniform code this fails.
#[test]
fn test_mixed_text_and_binary_params() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let scaled = Numeric::new(123_456, 2); // text-format param
    let count = 7_i32; // binary-format param
    let label = "widget"; // binary-format param

    let rows = test
        .connection
        .query_params(
            "SELECT CAST($1 AS NUMERIC(10,2)) AS amount, $2 AS qty, $3 AS label",
            &[
                &scaled as &dyn ToSqlParam,
                &count as &dyn ToSqlParam,
                &label as &dyn ToSqlParam,
            ],
        )
        .expect("query_params failed")
        .collect_rows()
        .expect("a mixed text/binary format array must be accepted");

    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<Numeric>(0).expect("non-NULL").to_string(),
        "1234.56"
    );
    assert_eq!(rows[0].get::<i32>(1), Some(7));
    assert_eq!(rows[0].get::<String>(2).as_deref(), Some("widget"));
}

/// `Geography` params round-trip as WKT — issue #133.
///
/// Previously impossible: Hyper has no PG-binary input function for
/// `geography` (`42883`), and every param went out as binary.
#[test]
fn test_geography_param_round_trip() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // Vertex-only geometries render back exactly; Hyper prints 7 fractional
    // digits. (Geometries with edges are densified along the great circle —
    // see the LINESTRING case below.)
    let cases: &[(&str, &str)] = &[
        ("POINT(-122.4194 37.7749)", "POINT(-122.4194000 37.7749000)"),
        (
            "MULTIPOINT(0 0, 1 1)",
            "MULTIPOINT((0.0000000 0.0000000), (1.0000000 1.0000000))",
        ),
    ];

    for &(wkt_in, expected) in cases {
        let geo = Geography::from_wkt(wkt_in).expect("Failed to create geography from WKT");
        let rows = test
            .connection
            .query_params("SELECT CAST($1 AS TEXT) AS wkt", &[&geo as &dyn ToSqlParam])
            .expect("query_params failed")
            .collect_rows()
            .unwrap_or_else(|e| panic!("Geography {wkt_in} must bind as a param, got: {e}"));

        assert_eq!(rows.len(), 1, "Expected exactly one row for {wkt_in}");
        assert_eq!(
            rows[0].get::<String>(0).as_deref(),
            Some(expected),
            "Geography should round-trip as the same geometry"
        );
    }

    // A LINESTRING is a geodesic: Hyper interpolates intermediate vertices
    // along the great circle, so only the endpoints are directly comparable.
    let line = Geography::from_wkt("LINESTRING(0 0, 2 2)").expect("valid WKT");
    let rows = test
        .connection
        .query_params(
            "SELECT CAST($1 AS TEXT) AS wkt",
            &[&line as &dyn ToSqlParam],
        )
        .expect("query_params failed")
        .collect_rows()
        .expect("LINESTRING Geography must bind as a param");
    let rendered = rows[0].get::<String>(0).expect("non-NULL");
    assert!(
        rendered.starts_with("LINESTRING(0.0000000 0.0000000, ")
            && rendered.ends_with("2.0000000 2.0000000)"),
        "LINESTRING endpoints should survive the round trip, got: {rendered}"
    );
}

/// A `Geography` param used as a predicate against a stored geography
/// column, with the column populated through the (independent) inserter
/// path — WKT in via `query_params`, matching the same geometry.
#[test]
fn test_geography_param_predicate() {
    let test = TestConnection::new().expect("Failed to create test connection");
    test.connection
        .execute_command("CREATE TABLE places (id INT, loc TABLEAU.TABGEOGRAPHY)")
        .expect("CREATE TABLE failed");

    let sf = Geography::from_wkt("POINT(-122.4194 37.7749)").expect("valid WKT");
    let nyc = Geography::from_wkt("POINT(-74.0060 40.7128)").expect("valid WKT");

    test.connection
        .command_params(
            "INSERT INTO places VALUES (1, $1)",
            &[&sf as &dyn ToSqlParam],
        )
        .expect("Geography must bind as an INSERT value");
    test.connection
        .command_params(
            "INSERT INTO places VALUES (2, $1)",
            &[&nyc as &dyn ToSqlParam],
        )
        .expect("Geography must bind as an INSERT value");

    let rows = test
        .connection
        .query_params(
            "SELECT id FROM places WHERE loc = $1",
            &[&sf as &dyn ToSqlParam],
        )
        .expect("query_params failed")
        .collect_rows()
        .expect("Geography must bind in a WHERE clause");

    assert_eq!(rows.len(), 1, "only the San Francisco row should match");
    assert_eq!(rows[0].get::<i32>(0), Some(1));
}

/// A `Geography` in Hyper's legacy binary format has no client-side WKT
/// rendering, so binding one must fail loudly rather than store garbage.
#[test]
fn test_geography_hyper_legacy_param_rejected() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let legacy = Geography::from_bytes(vec![0x01, 0x02, 0x03]);
    let err = test
        .connection
        .query_params("SELECT CAST($1 AS TEXT)", &[&legacy as &dyn ToSqlParam])
        .expect("query_params itself should not error")
        .collect_rows()
        .expect_err("legacy-format Geography must be rejected, not silently bound");

    let msg = err.to_string();
    assert!(
        msg.contains("22P02") || msg.contains("invalid geography format"),
        "expected Hyper's geography parse error (fail-fast), got: {msg}"
    );
}

// =============================================================================
// Async parity — the format-code array is plumbed through both stacks.
// =============================================================================

async fn fresh_async_conn(name: &str) -> Result<(HyperProcess, AsyncConnection)> {
    let db_path = common::test_result_path(name, "hyper")?;
    let params = common::test_hyper_params(name)?;
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper.require_endpoint()?.to_string();
    let conn = AsyncConnection::connect(
        &endpoint,
        db_path.to_str().expect("path"),
        CreateMode::CreateAndReplace,
    )
    .await?;
    Ok((hyper, conn))
}

/// Async twin of `test_numeric_scaled_param_round_trip` / `_against_column`.
#[tokio::test(flavor = "current_thread")]
async fn test_async_numeric_scaled_param() {
    let (_hyper, conn) = fresh_async_conn("async_numeric_scaled_param")
        .await
        .expect("async connection");

    let amount = Numeric::new(123_456, 2); // 1234.56
    let rows = conn
        .query_params(
            "SELECT CAST($1 AS NUMERIC(10,2)) AS v",
            &[&amount as &dyn ToSqlParam],
        )
        .await
        .expect("query_params failed")
        .collect_rows()
        .await
        .expect("scaled Numeric must bind on the async path too");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<Numeric>(0).expect("non-NULL").to_string(),
        "1234.56"
    );

    conn.execute_command("CREATE TABLE prices (id INT, amount NUMERIC(10,2))")
        .await
        .expect("CREATE TABLE failed");
    let inserted = conn
        .command_params(
            "INSERT INTO prices VALUES (1, $1)",
            &[&amount as &dyn ToSqlParam],
        )
        .await
        .expect("command_params must accept a scaled Numeric");
    assert_eq!(inserted, 1);
}

/// Async twin of `test_geography_param_round_trip`.
#[tokio::test(flavor = "current_thread")]
async fn test_async_geography_param() {
    let (_hyper, conn) = fresh_async_conn("async_geography_param")
        .await
        .expect("async connection");

    let geo = Geography::from_wkt("POINT(-122.4194 37.7749)").expect("valid WKT");
    let rows = conn
        .query_params("SELECT CAST($1 AS TEXT) AS wkt", &[&geo as &dyn ToSqlParam])
        .await
        .expect("query_params failed")
        .collect_rows()
        .await
        .expect("Geography must bind on the async path too");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<String>(0).as_deref(),
        Some("POINT(-122.4194000 37.7749000)")
    );
}

// =============================================================================
// Prepared-statement path
//
// `sql_oid()` is consulted only by the one-shot `query_params` /
// `command_params` path. A prepared statement fixes its parameter OIDs at
// `prepare_typed` time, so these tests pin what the two text-format types can
// and cannot do there.
// =============================================================================

/// `Geography` works unchanged through `PreparedStatement`: it declares a
/// concrete OID and always binds as text, so nothing depends on the value.
#[test]
fn test_geography_param_through_prepared_statement() {
    let test = TestConnection::new().expect("Failed to create test connection");
    let conn = &test.connection;

    conn.execute_command("CREATE TABLE places (id INT, location GEOGRAPHY)")
        .expect("CREATE TABLE failed");

    let stmt = conn
        .prepare_typed(
            "INSERT INTO places VALUES ($1, $2)",
            &[oids::INT, oids::GEOGRAPHY],
        )
        .expect("prepare_typed with a GEOGRAPHY parameter must succeed");

    // Reuse across executions is the whole point of a prepared statement.
    for (id, wkt) in [
        (1_i32, "POINT(-122.4194 37.7749)"),
        (2, "POINT(2.3522 48.8566)"),
        (3, "LINESTRING(0 0, 1 1)"),
    ] {
        let geo = Geography::from_wkt(wkt).expect("valid WKT");
        let inserted = stmt
            .execute(&[&id as &dyn ToSqlParam, &geo as &dyn ToSqlParam])
            .expect("Geography must bind through a prepared statement");
        assert_eq!(inserted, 1);
    }

    let rows = conn
        .execute_query("SELECT CAST(location AS TEXT) FROM places ORDER BY id")
        .expect("query failed")
        .collect_rows()
        .expect("collect_rows failed");
    let stored: Vec<String> = rows
        .iter()
        .map(|r| r.get::<String>(0).expect("non-NULL"))
        .collect();
    assert_eq!(stored[0], "POINT(-122.4194000 37.7749000)");
    assert_eq!(stored[1], "POINT(2.3522000 48.8566000)");
    // Hyper stores a linestring geodesically and hands back the densified
    // vertex list, so only the endpoints are stable.
    assert!(
        stored[2].starts_with("LINESTRING(0.0000000 0.0000000, ")
            && stored[2].ends_with("1.0000000 1.0000000)"),
        "linestring endpoints must survive the round-trip: {}",
        stored[2]
    );
}

/// Async twin of `test_geography_param_through_prepared_statement`.
#[tokio::test(flavor = "current_thread")]
async fn test_async_geography_param_through_prepared_statement() {
    let (_hyper, conn) = fresh_async_conn("async_geo_prepared")
        .await
        .expect("async connection");

    conn.execute_command("CREATE TABLE places (id INT, location GEOGRAPHY)")
        .await
        .expect("CREATE TABLE failed");

    let stmt = conn
        .prepare_typed(
            "INSERT INTO places VALUES ($1, $2)",
            &[oids::INT, oids::GEOGRAPHY],
        )
        .await
        .expect("prepare_typed with a GEOGRAPHY parameter must succeed");

    let geo = Geography::from_wkt("POINT(-122.4194 37.7749)").expect("valid WKT");
    let id = 1_i32;
    let inserted = stmt
        .execute(&[&id as &dyn ToSqlParam, &geo as &dyn ToSqlParam])
        .await
        .expect("Geography must bind through an async prepared statement");
    assert_eq!(inserted, 1);

    let rows = conn
        .execute_query("SELECT CAST(location AS TEXT) FROM places")
        .await
        .expect("query failed")
        .collect_rows()
        .await
        .expect("collect_rows failed");
    assert_eq!(
        rows[0].get::<String>(0).as_deref(),
        Some("POINT(-122.4194000 37.7749000)")
    );
}

/// A scaled `Numeric` binds through a prepared statement only when the
/// statement declared the parameter OID as unspecified (`0`).
///
/// This documents a real limitation rather than asserting a bug: because
/// parameter OIDs are fixed at `prepare_typed` time, before any value exists,
/// no single declaration serves both scale classes. `query_params` picks the
/// OID from the value and handles both — see `ToSqlParam for Numeric`.
#[test]
fn test_numeric_scale_classes_are_exclusive_on_the_prepared_path() {
    let test = TestConnection::new().expect("Failed to create test connection");
    let conn = &test.connection;

    conn.execute_command("CREATE TABLE prices (amount NUMERIC(10,2))")
        .expect("CREATE TABLE failed");

    let whole = Numeric::new(7, 0);
    let scaled = Numeric::new(123_456, 2); // 1234.56

    // Unspecified OID: scaled values work, whole numbers do not.
    let inferred = conn
        .prepare_typed("INSERT INTO prices VALUES ($1)", &[Oid::new(0)])
        .expect("prepare_typed failed");
    assert_eq!(
        inferred
            .execute(&[&scaled as &dyn ToSqlParam])
            .expect("a scaled Numeric must bind under an unspecified OID"),
        1
    );
    let err = inferred
        .execute(&[&whole as &dyn ToSqlParam])
        .expect_err("a whole Numeric cannot bind under an unspecified OID");
    assert!(
        err.to_string().contains("0A000"),
        "expected 0A000 truncation error, got: {err}"
    );

    // Declared NUMERIC: the mirror image.
    let declared = conn
        .prepare_typed("INSERT INTO prices VALUES ($1)", &[oids::NUMERIC])
        .expect("prepare_typed failed");
    assert_eq!(
        declared
            .execute(&[&whole as &dyn ToSqlParam])
            .expect("a whole Numeric must bind under a declared NUMERIC OID"),
        1
    );
    let err = declared
        .execute(&[&scaled as &dyn ToSqlParam])
        .expect_err("a scaled Numeric cannot bind under a declared NUMERIC OID");
    assert!(
        err.to_string().contains("22003"),
        "expected 22003 numeric overflow, got: {err}"
    );

    // The one-shot path has no such split: it reads the OID off the value.
    for value in [&whole, &scaled] {
        conn.command_params(
            "INSERT INTO prices VALUES ($1)",
            &[value as &dyn ToSqlParam],
        )
        .expect("query_params handles both scale classes");
    }
}

/// `Connection::prepare` passes an empty OID list, and Hyper does not infer
/// parameter types at Parse time — so any `$N` is rejected outright.
///
/// Pinned because it is the first thing a user hits, and it is why
/// `prepare_typed` is the only route for a parameterized prepared statement.
#[test]
fn test_untyped_prepare_rejects_parameters() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let err = test
        .connection
        .prepare("SELECT $1")
        .expect_err("untyped prepare cannot declare a parameter");
    assert!(
        err.to_string().contains("42601"),
        "expected 42601 unexpected-parameter error, got: {err}"
    );
}

/// Date, Timestamp and OffsetTimestamp parameters must round-trip: the
/// bound value must be read back by the server as the same instant.
/// Regression: they were encoded with the absolute Julian epoch instead of
/// the PostgreSQL 2000-01-01 epoch, so the server stored them ~6,700 years off.
#[test]
fn test_date_time_params_round_trip() {
    use hyperdb_api::{Date, OffsetTimestamp, Time, Timestamp};

    let test = TestConnection::new().expect("Failed to create test connection");

    let date = Date::new(2024, 1, 15);
    let timestamp = Timestamp::new(Date::new(2024, 1, 15), Time::new(13, 45, 30, 123_456));
    let offset_ts = OffsetTimestamp::new(timestamp, 0);

    let result = test
        .connection
        .query_params(
            "SELECT CAST($1 AS text), CAST($2 AS text), \
                    $3 = TIMESTAMPTZ '2024-01-15 13:45:30.123456+00'",
            &[
                &date as &dyn ToSqlParam,
                &timestamp as &dyn ToSqlParam,
                &offset_ts as &dyn ToSqlParam,
            ],
        )
        .expect("query_params failed");
    let rows = result.collect_rows().expect("collect_rows failed");
    assert_eq!(rows.len(), 1);

    let d: Option<String> = rows[0].get(0);
    let ts: Option<String> = rows[0].get(1);
    // TIMESTAMPTZ text renders in the session time zone, so compare the instant.
    let same_instant: Option<bool> = rows[0].get(2);
    assert_eq!(d.as_deref(), Some("2024-01-15"));
    assert_eq!(ts.as_deref(), Some("2024-01-15 13:45:30.123456"));
    assert_eq!(same_instant, Some(true), "timestamptz instant mismatch");
}
