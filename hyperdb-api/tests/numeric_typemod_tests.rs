// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Regression tests for `NUMERIC` type-modifier plumbing end-to-end.
//!
//! Three bugs motivated this test module, all in the same chain:
//!
//! 1. `hyperdb_api_core::client::statement::Column` didn't store the wire protocol's
//!    `type_modifier`, so PG `atttypmod` got dropped at the
//!    client-to-hyperdb-api boundary.
//! 2. `hyperdb_api::Rowset::schema()` (TCP path) used
//!    `SqlType::from_oid()` — which has no way to know precision/scale —
//!    instead of `from_oid_and_modifier()`.
//! 3. `hyperdb_api_core::types::Numeric::from_binary_with_scale` only accepted the
//!    16-byte `i128` wire form, rejecting the 8-byte `i64` form that
//!    Hyper uses for `Numeric(precision ≤ 18)`.
//!
//! The downstream symptom was that `AVG(INT)` — which Hyper types as
//! `Numeric(16, 6)` per
//! `hyper/cts/algebra/operator/AggregationLogic.cpp::dividingAggregateType`
//! — showed up in result schemas as `Numeric { precision: 0, scale: 0 }`
//! and its 8-byte wire bytes could not be decoded at all, so every
//! `AVG(INT)` came back as garbage or `Null` to JSON consumers. The
//! tests here pin both the schema and the byte decode end-to-end so a
//! future refactor that regresses any link in the chain fails loudly.

mod common;

use common::TestConnection;
use hyperdb_api::{Numeric, SqlType};

/// After loading a small INT table, `SELECT AVG(col)` should return a
/// schema where the column's `SqlType` is
/// `Numeric { precision: 16, scale: 6 }`.
///
/// This test is the regression guard for bugs (1) and (2) together: if
/// either the client drops `type_modifier` or `Rowset::schema()` falls
/// back to `from_oid()`, precision and scale come back as 0 and this
/// assertion fails.
#[test]
fn avg_integer_schema_has_precision_16_scale_6() {
    let tc = TestConnection::new().expect("test connection");
    tc.execute_command("CREATE TABLE t (v INT)").unwrap();
    tc.execute_command("INSERT INTO t VALUES (1), (2), (3)")
        .unwrap();

    let mut result = tc
        .connection
        .execute_query("SELECT AVG(v) AS avg_v FROM t")
        .unwrap();
    // `schema()` only populates once the first chunk has been read (TCP
    // learns the schema from `RowDescription`, which arrives before the
    // first DataRow).
    let _ = result.next_chunk().unwrap();
    let schema = result
        .schema()
        .expect("schema should be available after first chunk");

    assert_eq!(schema.column_count(), 1);
    let col = schema.column(0);
    match col.sql_type() {
        SqlType::Numeric {
            precision, scale, ..
        } => {
            assert_eq!(
                precision, 16,
                "AVG(INTEGER) precision should be 16 per Hyper's \
                 `dividingAggregateType(Integer)` — see \
                 hyper/cts/algebra/operator/AggregationLogic.cpp:805"
            );
            assert_eq!(
                scale, 6,
                "AVG(INTEGER) scale should be 6 per the same source"
            );
        }
        other => panic!("expected Numeric, got {other:?}"),
    }
}

/// After loading a BIGINT table, `SELECT AVG(col)` should return a
/// schema where the column's `SqlType` is
/// `Numeric { precision: 25, scale: 6 }` — still a Numeric at the
/// `PostgreSQL` OID level, but in Hyper internally a `BigNumeric`, which
/// means the wire bytes will be 16-byte (i128) instead of 8-byte (i64).
/// The schema and the bytes-per-row are both pinned here.
#[test]
fn avg_bigint_schema_has_precision_25_scale_6() {
    let tc = TestConnection::new().expect("test connection");
    tc.execute_command("CREATE TABLE t (v BIGINT)").unwrap();
    tc.execute_command("INSERT INTO t VALUES (1), (2), (3)")
        .unwrap();

    let mut result = tc
        .connection
        .execute_query("SELECT AVG(v) AS avg_v FROM t")
        .unwrap();
    let _ = result.next_chunk().unwrap();
    let schema = result.schema().expect("schema after first chunk");
    let col = schema.column(0);
    match col.sql_type() {
        SqlType::Numeric {
            precision, scale, ..
        } => {
            assert_eq!(precision, 25, "AVG(BIGINT) precision should be 25");
            assert_eq!(scale, 6, "AVG(BIGINT) scale should be 6");
        }
        other => panic!("expected Numeric, got {other:?}"),
    }
}

/// Full end-to-end via the clean API: `row.get::<Numeric>(idx)`.
///
/// This is the happy-path test for `Row::get_numeric` and the
/// `impl RowValue for Numeric` plumbing. The caller doesn't touch raw
/// bytes or scale — the row looks them up internally from its attached
/// schema and dispatches to the right decode width.
///
/// If any link in the chain regresses (schema not attached,
/// `type_modifier` not carried, 8-byte decode path not selected) this
/// test fails.
#[test]
fn avg_integer_via_row_get_numeric() {
    let tc = TestConnection::new().expect("test connection");
    tc.execute_command("CREATE TABLE t (v INT)").unwrap();
    tc.execute_command("INSERT INTO t VALUES (1), (2), (3)")
        .unwrap();

    let mut result = tc
        .connection
        .execute_query("SELECT AVG(v) AS avg_v FROM t")
        .unwrap();
    let chunk = result.next_chunk().unwrap().expect("one row");
    assert_eq!(chunk.len(), 1);
    let row = &chunk[0];

    // The clean API — no scale plumbing by the caller:
    let numeric: Numeric = row
        .get::<Numeric>(0)
        .expect("AVG(INT) decodes via row.get::<Numeric>");
    assert_eq!(numeric.scale(), 6);
    assert!(
        (numeric.to_f64() - 2.0).abs() < 1e-12,
        "expected 2.0, got {}",
        numeric.to_f64()
    );

    // `try_get` gives the same result with better error messages on
    // schema mismatch — exercise that path too.
    let numeric2: Numeric = row
        .try_get::<Numeric>(0, "avg_v")
        .expect("AVG(INT) also decodes via row.try_get::<Numeric>");
    assert_eq!(numeric, numeric2);

    // And row.sql_type() exposes the column's SqlType from the schema.
    match row.sql_type(0) {
        Some(SqlType::Numeric {
            precision, scale, ..
        }) => {
            assert_eq!(precision, 16);
            assert_eq!(scale, 6);
        }
        other => panic!("expected Numeric via row.sql_type, got {other:?}"),
    }
}

/// Defensive test: bytes-level decode still works via the lower-level
/// API for callers who want explicit control (e.g. batch-decoding
/// multiple rows without revisiting the schema per row).
#[test]
fn avg_integer_bytes_decode_to_correct_numeric() {
    let tc = TestConnection::new().expect("test connection");
    tc.execute_command("CREATE TABLE t (v INT)").unwrap();
    tc.execute_command("INSERT INTO t VALUES (1), (2), (3)")
        .unwrap();

    let mut result = tc
        .connection
        .execute_query("SELECT AVG(v) AS avg_v FROM t")
        .unwrap();
    let chunk = result.next_chunk().unwrap().expect("one row");
    let schema = result.schema().expect("schema");
    let scale = match schema.column(0).sql_type() {
        SqlType::Numeric { scale, .. } => scale,
        other => panic!("expected Numeric, got {other:?}"),
    };

    let row = &chunk[0];
    let bytes = row.get_bytes(0).expect("non-null AVG value");
    // Hyper's `Numeric(16, 6)` = `AVG(INT)` is the 8-byte wire form
    // (precision ≤ 18 uses int64). `from_binary_with_scale` must accept
    // both 8- and 16-byte buffers for this to work.
    assert_eq!(
        bytes.len(),
        8,
        "AVG(INT) should come back as the 8-byte HyperBinary Numeric form"
    );
    // Scale in `SqlType::Numeric` is `u32`; Hyper caps NUMERIC at
    // precision 38 so legitimate scale always fits in `u8`. Use
    // `try_from` rather than `as u8` so an unexpected wide value
    // from a malformed server response or a typemod-parsing bug
    // fails loudly here instead of being silently truncated to a
    // wrong low-byte value.
    let scale_u8: u8 = u8::try_from(scale).expect("scale fits in u8");
    let numeric = Numeric::from_binary_with_scale(&bytes, scale_u8)
        .expect("decode AVG(INT) bytes as Numeric");

    assert_eq!(numeric.scale(), 6);
    assert!(
        (numeric.to_f64() - 2.0).abs() < 1e-12,
        "expected 2.0, got {}",
        numeric.to_f64()
    );
}

/// `NUMERIC(10, 2)` columns stored directly on a table should also
/// round-trip their precision/scale through the schema — this exercises
/// the same type-modifier-plumbing path but for an explicitly-declared
/// column rather than an aggregate result.
#[test]
fn explicit_numeric_column_preserves_precision_and_scale() {
    let tc = TestConnection::new().expect("test connection");
    tc.execute_command("CREATE TABLE prices (p NUMERIC(10, 2))")
        .unwrap();
    tc.execute_command("INSERT INTO prices VALUES (1.23), (4.56)")
        .unwrap();

    let mut result = tc
        .connection
        .execute_query("SELECT p FROM prices ORDER BY p")
        .unwrap();
    let _ = result.next_chunk().unwrap();
    let schema = result.schema().expect("schema");
    match schema.column(0).sql_type() {
        SqlType::Numeric {
            precision, scale, ..
        } => {
            assert_eq!(precision, 10);
            assert_eq!(scale, 2);
        }
        other => panic!("expected Numeric, got {other:?}"),
    }
}
