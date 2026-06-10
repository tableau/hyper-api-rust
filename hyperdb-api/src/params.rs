// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Parameter encoding for parameterized queries.
//!
//! This module provides the [`ToSqlParam`] trait for type-safe parameter encoding
//! in parameterized SQL queries, preventing SQL injection attacks.
//!
//! # SQL Injection Prevention
//!
//! Using parameterized queries is the safest way to include user input in SQL:
//!
//! ```no_run
//! # use hyperdb_api::{Connection, Result};
//! # fn example(conn: &Connection, user_input: &str) -> Result<()> {
//! // DANGEROUS - vulnerable to SQL injection:
//! let query = format!("SELECT * FROM users WHERE name = '{}'", user_input);
//!
//! // SAFE - parameterized query:
//! let result = conn.query_params("SELECT * FROM users WHERE name = $1", &[&user_input])?;
//! # Ok(())
//! # }
//! ```
//!
//! # Supported Types
//!
//! The following types implement [`ToSqlParam`]:
//!
//! - Integers: `i16`, `i32`, `i64`
//! - Floats: `f32`, `f64`
//! - `bool`
//! - `&str`, `String`
//! - Bytes: `&[u8]`, `Vec<u8>`
//! - Date/time types: `Date`, `Time`, `Timestamp`, `OffsetTimestamp`
//! - `Interval`
//! - `Numeric` — **whole numbers only (`scale == 0`)**; Hyper rejects
//!   scaled binary NUMERIC params (see the `Numeric` impl and issue #132)
//! - `serde_json::Value` (binds as PostgreSQL `json`)
//! - `Option<T>` where `T: ToSqlParam` (for nullable parameters)
//! - `&T` where `T: ToSqlParam`
//!
//! Note: `Geography` does **not** implement `ToSqlParam` — Hyper has no
//! PostgreSQL-binary input function for the geography type (issue #133).
//! Use the [`Inserter`](crate::Inserter) (`IntoValue`) path to write
//! geography values instead.
//!
//! # Example
//!
//! ```no_run
//! use hyperdb_api::{Connection, CreateMode, ToSqlParam, Result};
//!
//! fn find_user(conn: &Connection, user_id: i32, name: &str) -> Result<()> {
//!     // Multiple parameters with different types
//!     let result = conn.query_params(
//!         "SELECT * FROM users WHERE id = $1 AND name = $2",
//!         &[&user_id, &name],
//!     )?;
//!     Ok(())
//! }
//! ```

use hyperdb_api_core::types::{
    oids, Date, Interval, Numeric, OffsetTimestamp, Oid, Time, Timestamp,
};

/// Trait for types that can be used as parameters in parameterized SQL queries.
///
/// This trait enables type-safe parameter encoding for use with
/// [`Connection::query_params`](crate::Connection::query_params) and
/// [`Connection::command_params`](crate::Connection::command_params).
///
/// # Implementing for Custom Types
///
/// You can implement this trait for custom types:
///
/// ```no_run
/// # use hyperdb_api::ToSqlParam;
/// # struct MyType;
/// # impl MyType { fn to_bytes(&self) -> Vec<u8> { vec![] } }
/// # impl ToString for MyType { fn to_string(&self) -> String { String::new() } }
/// impl ToSqlParam for MyType {
///     fn encode_param(&self) -> Option<Vec<u8>> {
///         Some(self.to_bytes())
///     }
///
///     fn to_sql_literal(&self) -> String {
///         format!("'{}'", self.to_string().replace('\'', "''"))
///     }
/// }
/// ```
pub trait ToSqlParam: Send + Sync {
    /// Encodes this value as binary bytes for use in parameterized queries.
    ///
    /// Returns `None` to represent a SQL NULL value.
    /// Returns `Some(bytes)` with the binary-encoded value otherwise.
    fn encode_param(&self) -> Option<Vec<u8>>;

    /// Returns the SQL type OID this parameter should bind as.
    ///
    /// The default returns `Oid(0)` (unspecified) which asks the server
    /// to infer the type from surrounding SQL context. That works for
    /// clauses like `WHERE column = $1` where the column type is known,
    /// but not for `INSERT INTO t VALUES ($1, $2)` — those require the
    /// caller (or the trait impl) to return a concrete OID.
    ///
    /// All built-in `ToSqlParam` impls override this with a concrete
    /// value from [`hyperdb_api_core::types::oids`].
    fn sql_oid(&self) -> Oid {
        Oid::new(0)
    }

    /// Returns the SQL literal representation of this value.
    ///
    /// Retained for building DDL statement strings that cannot use
    /// parameterized queries (e.g. `escape_sql_path` in catalog code).
    /// The parameterized-query path in
    /// [`Connection::query_params`](crate::Connection::query_params)
    /// no longer uses this method — parameters travel as binary bytes
    /// via `encode_param`.
    fn to_sql_literal(&self) -> String;
}

// =============================================================================
// Integer implementations
// =============================================================================

impl ToSqlParam for i16 {
    fn encode_param(&self) -> Option<Vec<u8>> {
        // PostgreSQL wire-protocol Bind uses big-endian for numeric
        // binary parameters. (Results come back as little-endian
        // HyperBinary because we request format code 2 for results;
        // params use format code 1 = standard PG binary = BE.)
        Some(self.to_be_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::SMALL_INT
    }

    fn to_sql_literal(&self) -> String {
        self.to_string()
    }
}

impl ToSqlParam for i32 {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(self.to_be_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::INT
    }

    fn to_sql_literal(&self) -> String {
        self.to_string()
    }
}

impl ToSqlParam for i64 {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(self.to_be_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::BIG_INT
    }

    fn to_sql_literal(&self) -> String {
        self.to_string()
    }
}

// =============================================================================
// Float implementations
// =============================================================================

impl ToSqlParam for f32 {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(self.to_be_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::FLOAT
    }

    fn to_sql_literal(&self) -> String {
        // Handle special float values
        if self.is_nan() {
            "'NaN'".to_string()
        } else if self.is_infinite() {
            if *self > 0.0 {
                "'Infinity'".to_string()
            } else {
                "'-Infinity'".to_string()
            }
        } else {
            self.to_string()
        }
    }
}

impl ToSqlParam for f64 {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(self.to_be_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::DOUBLE
    }

    fn to_sql_literal(&self) -> String {
        // Handle special float values
        if self.is_nan() {
            "'NaN'".to_string()
        } else if self.is_infinite() {
            if *self > 0.0 {
                "'Infinity'".to_string()
            } else {
                "'-Infinity'".to_string()
            }
        } else {
            self.to_string()
        }
    }
}

// =============================================================================
// Boolean implementation
// =============================================================================

impl ToSqlParam for bool {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(vec![u8::from(*self)])
    }

    fn sql_oid(&self) -> Oid {
        oids::BOOL
    }

    fn to_sql_literal(&self) -> String {
        if *self { "TRUE" } else { "FALSE" }.to_string()
    }
}

// =============================================================================
// String implementations
// =============================================================================

impl ToSqlParam for str {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(self.as_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::TEXT
    }

    fn to_sql_literal(&self) -> String {
        // Escape single quotes by doubling them
        format!("'{}'", self.replace('\'', "''"))
    }
}

impl ToSqlParam for String {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(self.as_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::TEXT
    }

    fn to_sql_literal(&self) -> String {
        format!("'{}'", self.replace('\'', "''"))
    }
}

impl ToSqlParam for &str {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(self.as_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::TEXT
    }

    fn to_sql_literal(&self) -> String {
        format!("'{}'", self.replace('\'', "''"))
    }
}

// =============================================================================
// Reference implementations
// =============================================================================

impl<T: ToSqlParam> ToSqlParam for &T {
    fn encode_param(&self) -> Option<Vec<u8>> {
        (*self).encode_param()
    }

    fn sql_oid(&self) -> Oid {
        (*self).sql_oid()
    }

    fn to_sql_literal(&self) -> String {
        (*self).to_sql_literal()
    }
}

// =============================================================================
// Option implementation (for nullable parameters)
// =============================================================================

impl<T: ToSqlParam> ToSqlParam for Option<T> {
    fn encode_param(&self) -> Option<Vec<u8>> {
        match self {
            Some(value) => value.encode_param(),
            None => None, // SQL NULL
        }
    }

    fn sql_oid(&self) -> Oid {
        match self {
            Some(value) => value.sql_oid(),
            // For NULL we leave the OID unspecified — server infers
            // from context, which is the correct behavior for `WHERE
            // col = $1` with a NULL binding.
            None => Oid::new(0),
        }
    }

    fn to_sql_literal(&self) -> String {
        match self {
            Some(value) => value.to_sql_literal(),
            None => "NULL".to_string(),
        }
    }
}

// =============================================================================
// Date/Time implementations
// =============================================================================

impl ToSqlParam for Date {
    fn encode_param(&self) -> Option<Vec<u8>> {
        // Date is stored as i32 Julian day offset from 2000-01-01.
        // Big-endian per the PG Bind protocol (format code 1).
        Some(self.to_julian_day().to_be_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::DATE
    }

    fn to_sql_literal(&self) -> String {
        format!("DATE '{self}'")
    }
}

impl ToSqlParam for Time {
    fn encode_param(&self) -> Option<Vec<u8>> {
        // Time is stored as i64 microseconds since midnight.
        Some(self.to_microseconds().to_be_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::TIME
    }

    fn to_sql_literal(&self) -> String {
        format!("TIME '{self}'")
    }
}

impl ToSqlParam for Timestamp {
    fn encode_param(&self) -> Option<Vec<u8>> {
        // Timestamp is stored as i64 microseconds since 2000-01-01.
        Some(self.to_microseconds().to_be_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::TIMESTAMP
    }

    fn to_sql_literal(&self) -> String {
        format!("TIMESTAMP '{self}'")
    }
}

impl ToSqlParam for OffsetTimestamp {
    fn encode_param(&self) -> Option<Vec<u8>> {
        // OffsetTimestamp is stored as i64 microseconds UTC since 2000-01-01.
        Some(self.to_microseconds_utc().to_be_bytes().to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::TIMESTAMP_TZ
    }

    fn to_sql_literal(&self) -> String {
        format!("TIMESTAMPTZ '{self}'")
    }
}

// =============================================================================
// Bytes implementation
// =============================================================================

impl ToSqlParam for [u8] {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(self.to_vec())
    }

    fn sql_oid(&self) -> Oid {
        oids::BYTE_A
    }

    #[expect(
        clippy::format_collect,
        reason = "readable hex/string formatting loop; refactoring to fold! obscures intent"
    )]
    fn to_sql_literal(&self) -> String {
        // Encode as hex bytea literal
        let hex_str: String = self.iter().map(|b| format!("{b:02x}")).collect();
        format!("E'\\\\x{hex_str}'")
    }
}

impl ToSqlParam for Vec<u8> {
    fn encode_param(&self) -> Option<Vec<u8>> {
        Some(self.clone())
    }

    fn sql_oid(&self) -> Oid {
        oids::BYTE_A
    }

    #[expect(
        clippy::format_collect,
        reason = "readable hex/string formatting loop; refactoring to fold! obscures intent"
    )]
    fn to_sql_literal(&self) -> String {
        let hex_str: String = self.iter().map(|b| format!("{b:02x}")).collect();
        format!("E'\\\\x{hex_str}'")
    }
}

// =============================================================================
// Numeric implementation
// =============================================================================

/// Encode a whole-number (`scale == 0`) `Numeric` as PostgreSQL binary NUMERIC.
///
/// Header (i16 BE): `ndigits`, `weight`, `sign` (0x0000 pos / 0x4000 neg),
/// `dscale = 0`; then `ndigits` base-10000 groups (i16 BE, most-significant
/// first). The `weight` of the most-significant group is `ndigits - 1` (it
/// sits at base-10000 position `ndigits-1`), and `dscale` is 0 because there
/// are no fractional digits.
///
/// This handles ONLY `scale == 0`. Correctly encoding a scaled NUMERIC
/// requires decomposing the *decimal* representation into base-10000 groups
/// aligned on the decimal point (not decomposing the unscaled integer) — that
/// is out of scope here because Hyper rejects scaled binary NUMERIC params
/// regardless (see [`ToSqlParam for Numeric`] and #132). The caller is
/// responsible for only invoking this with `scale == 0`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "an i128 spans at most ~39 decimal digits → ≤10 base-10000 groups; \
              ndigits and weight always fit in i16"
)]
fn pg_numeric_encode_unscaled(unscaled: i128) -> Vec<u8> {
    let sign_neg = unscaled < 0;
    let mut mag = unscaled.unsigned_abs();

    // Decompose the integer magnitude into base-10000 groups, least-significant
    // first, then reverse to most-significant first.
    let mut groups: Vec<i16> = Vec::new();
    while mag > 0 {
        groups.push((mag % 10000) as i16);
        mag /= 10000;
    }
    groups.reverse(); // empty when unscaled == 0

    let ndigits = groups.len() as i16;
    let weight = if groups.is_empty() { 0 } else { ndigits - 1 };

    let mut buf = Vec::with_capacity(8 + groups.len() * 2);
    buf.extend_from_slice(&ndigits.to_be_bytes());
    buf.extend_from_slice(&weight.to_be_bytes());
    buf.extend_from_slice(&(if sign_neg { 0x4000_i16 } else { 0 }).to_be_bytes());
    buf.extend_from_slice(&0_i16.to_be_bytes()); // dscale = 0 (whole number)
    for g in groups {
        buf.extend_from_slice(&g.to_be_bytes());
    }
    buf
}

impl ToSqlParam for Numeric {
    /// Binds as PostgreSQL binary NUMERIC. **Only `scale() == 0` (whole
    /// numbers) is supported.**
    ///
    /// Hyper rejects scaled binary NUMERIC params at query time with SQLSTATE
    /// `0A000` ("cannot handle truncation when reading numerics") — verified
    /// empirically, and regardless of an explicit `CAST`. So a faithful scaled
    /// encoder would never succeed anyway; full scaled support is tracked in
    /// #132.
    ///
    /// For `scale() > 0` this returns a header whose `dscale` is set to the
    /// true scale. The byte payload is therefore NOT a correct PostgreSQL
    /// NUMERIC for the value (correct scaled encoding requires decimal-aligned
    /// base-10000 grouping, deferred to #132) — but because `dscale > 0`, Hyper
    /// rejects it server-side before it can be misinterpreted. The net effect
    /// is fail-fast: a scaled param errors clearly instead of silently binding
    /// a wrong whole number.
    fn encode_param(&self) -> Option<Vec<u8>> {
        if self.scale() == 0 {
            return Some(pg_numeric_encode_unscaled(self.unscaled_value()));
        }
        // scale > 0: emit the unscaled digits but with dscale = scale so the
        // server rejects it (0A000) rather than reading a mis-scaled integer.
        // These bytes are intentionally server-rejected, not a valid value;
        // see the doc comment and #132.
        let mut buf = pg_numeric_encode_unscaled(self.unscaled_value());
        // Overwrite the dscale field (bytes 6..8) with the true scale.
        let dscale = i16::from(self.scale()).to_be_bytes();
        buf[6] = dscale[0];
        buf[7] = dscale[1];
        Some(buf)
    }
    fn sql_oid(&self) -> Oid {
        oids::NUMERIC
    }
    fn to_sql_literal(&self) -> String {
        self.to_string()
    } // Display = decimal string
}

// =============================================================================
// Interval implementation
// =============================================================================

impl ToSqlParam for Interval {
    fn encode_param(&self) -> Option<Vec<u8>> {
        // PG interval binary (Bind format code 1): i64 microseconds, i32 days,
        // i32 months — all BIG-endian. NB this differs from Hyper's HyperBinary
        // `Interval::encode()` which is the same field order but LITTLE-endian.
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(&self.microseconds().to_be_bytes());
        buf.extend_from_slice(&self.days().to_be_bytes());
        buf.extend_from_slice(&self.months().to_be_bytes());
        Some(buf)
    }
    fn sql_oid(&self) -> Oid {
        oids::INTERVAL
    }
    fn to_sql_literal(&self) -> String {
        format!("INTERVAL '{self}'")
    }
}

// =============================================================================
// JSON implementation
// =============================================================================

impl ToSqlParam for serde_json::Value {
    fn encode_param(&self) -> Option<Vec<u8>> {
        // PG `json` binary form == the UTF-8 text. (jsonb has a leading
        // version byte; `json` does not, and oids::JSON is `json`.)
        // Value::to_string() is compact (no whitespace, no trailing newline)
        // and correctly escapes embedded quotes — exactly the wire form needed.
        Some(self.to_string().into_bytes())
    }
    fn sql_oid(&self) -> Oid {
        oids::JSON
    }
    fn to_sql_literal(&self) -> String {
        format!("'{}'", self.to_string().replace('\'', "''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_i32_encoding() {
        // Big-endian per PG Bind format code 1.
        assert_eq!(42i32.encode_param(), Some(vec![0, 0, 0, 42]));
        assert_eq!((-1i32).encode_param(), Some(vec![255, 255, 255, 255]));
    }

    #[test]
    fn test_i64_encoding() {
        assert_eq!(42i64.encode_param(), Some(vec![0, 0, 0, 0, 0, 0, 0, 42]));
    }

    #[test]
    fn test_string_encoding() {
        assert_eq!("hello".encode_param(), Some(b"hello".to_vec()));
        assert_eq!(
            String::from("world").encode_param(),
            Some(b"world".to_vec())
        );
    }

    #[test]
    fn test_bool_encoding() {
        assert_eq!(true.encode_param(), Some(vec![1]));
        assert_eq!(false.encode_param(), Some(vec![0]));
    }

    #[test]
    fn test_option_encoding() {
        // Big-endian per PG Bind format code 1.
        assert_eq!(Some(42i32).encode_param(), Some(vec![0, 0, 0, 42]));
        assert_eq!(None::<i32>.encode_param(), None);
    }

    #[test]
    fn test_reference_encoding() {
        let value = 42i32;
        assert_eq!(value.encode_param(), Some(vec![0, 0, 0, 42]));
        assert_eq!((&&value).encode_param(), Some(vec![0, 0, 0, 42]));
    }

    #[test]
    fn test_pg_numeric_encode_unscaled() {
        // 42 → ndigits=1, weight=0, sign=0, dscale=0, group=42
        assert_eq!(
            pg_numeric_encode_unscaled(42),
            vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 42]
        );

        // 0 → ndigits=0, weight=0, sign=0, dscale=0 (empty digit list)
        assert_eq!(pg_numeric_encode_unscaled(0), vec![0, 0, 0, 0, 0, 0, 0, 0]);

        // -1 → ndigits=1, weight=0, sign=0x4000, dscale=0, group=1
        assert_eq!(
            pg_numeric_encode_unscaled(-1),
            vec![0, 1, 0, 0, 0x40, 0, 0, 0, 0, 1]
        );

        // 123456789 = 1*10000^2 + 2345*10000 + 6789
        // → ndigits=3, weight=2, sign=0, dscale=0, groups=[1, 2345, 6789]
        assert_eq!(
            pg_numeric_encode_unscaled(123_456_789),
            vec![
                0, 3, // ndigits=3
                0, 2, // weight=2
                0, 0, // sign=0
                0, 0, // dscale=0
                0, 1, // group 1
                9, 41, // group 2345 (0x0929)
                26, 133 // group 6789 (0x1A85)
            ]
        );
    }

    #[test]
    fn test_numeric_scale0_encode_param() {
        // The scale=0 ToSqlParam path produces the canonical whole-number form.
        assert_eq!(
            Numeric::new(42, 0).encode_param(),
            Some(vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 42])
        );
    }

    #[test]
    fn test_numeric_scaled_sets_dscale_for_rejection() {
        // For scale>0, encode_param sets dscale = true scale so the server
        // REJECTS the param (0A000). These bytes are intentionally NOT a valid
        // representation of 1.23 — correct scaled encoding is #132. We only
        // assert the dscale field (bytes 6..8) carries the scale, which is what
        // triggers Hyper's fail-fast rejection.
        let bytes = Numeric::new(123, 2).encode_param().expect("some");
        assert_eq!(&bytes[6..8], &[0, 2], "dscale must equal the true scale");
        assert_ne!(&bytes[6..8], &[0, 0], "must not look like a whole number");
    }

    #[test]
    fn test_interval_encoding() {
        // Interval::new(months, days, microseconds)
        let interval = Interval::new(2, 5, 0);
        // PG binary: [us:i64 BE][days:i32 BE][months:i32 BE]
        assert_eq!(
            interval.encode_param(),
            Some(vec![
                0, 0, 0, 0, 0, 0, 0, 0, // us = 0
                0, 0, 0, 5, // days = 5
                0, 0, 0, 2 // months = 2
            ])
        );
    }

    #[test]
    fn test_json_encoding() {
        let json = serde_json::json!({"a": 1});
        // UTF-8 bytes of compact JSON string
        assert_eq!(json.encode_param(), Some(br#"{"a":1}"#.to_vec()));
    }
}
