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
//! - `Numeric` (any scale)
//! - `Geography` (binds as WKT)
//! - `serde_json::Value` (binds as PostgreSQL `json`)
//! - `Option<T>` where `T: ToSqlParam` (for nullable parameters)
//! - `&T` where `T: ToSqlParam`
//!
//! # Wire format
//!
//! Parameters travel as PostgreSQL **binary** (format code `1`) by default.
//! Two types have no binary input function in Hyper and bind as **text**
//! (format code `0`) instead — see [`ParamFormat`]:
//!
//! - a `Numeric` with `scale() > 0`, and
//! - `Geography`.
//!
//! The `Bind` message carries a per-parameter format-code array, so a single
//! statement can mix both and every other parameter keeps the binary fast
//! path.
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
//!
//! # Mapping parameterized results into structs
//!
//! [`query_params`](crate::Connection::query_params) returns raw
//! [`Row`](crate::Row)s. To map a parameterized query's results straight into
//! a [`FromRow`](crate::FromRow) struct in one call, use the `_as_params`
//! variants — [`fetch_one_as_params`](crate::Connection::fetch_one_as_params),
//! [`fetch_all_as_params`](crate::Connection::fetch_all_as_params), and
//! [`stream_as_params`](crate::Connection::stream_as_params) (and their
//! [`AsyncConnection`](crate::AsyncConnection) equivalents).

pub use hyperdb_api_core::client::ParamFormat;
use hyperdb_api_core::types::{
    Date, Geography, Interval, Numeric, OffsetTimestamp, Oid, Time, Timestamp, oids,
};

/// Trait for types that can be used as parameters in parameterized SQL queries.
///
/// This trait enables type-safe parameter encoding for use with
/// [`Connection::query_params`](crate::Connection::query_params) and
/// [`Connection::command_params`](crate::Connection::command_params), and with
/// the struct-mapping variants
/// [`fetch_one_as_params`](crate::Connection::fetch_one_as_params),
/// [`fetch_all_as_params`](crate::Connection::fetch_all_as_params), and
/// [`stream_as_params`](crate::Connection::stream_as_params).
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
    /// Encodes this value as the wire bytes for a bound parameter.
    ///
    /// Returns `None` to represent a SQL NULL value.
    /// Returns `Some(bytes)` with the encoded value otherwise.
    ///
    /// The bytes must match whatever [`Self::param_format`] returns —
    /// big-endian PostgreSQL binary for [`ParamFormat::Binary`] (the
    /// default), UTF-8 SQL text without surrounding quotes for
    /// [`ParamFormat::Text`].
    fn encode_param(&self) -> Option<Vec<u8>>;

    /// Returns the wire format [`Self::encode_param`] produced.
    ///
    /// Defaults to [`ParamFormat::Binary`]. Override it only for types
    /// Hyper cannot read in binary — a scaled `Numeric` and `Geography`
    /// are the two built-in cases.
    fn param_format(&self) -> ParamFormat {
        ParamFormat::Binary
    }

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

    fn param_format(&self) -> ParamFormat {
        (*self).param_format()
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

    fn param_format(&self) -> ParamFormat {
        match self {
            Some(value) => value.param_format(),
            // A NULL carries a -1 length and no bytes, so its format code is
            // never consulted. Binary keeps the all-binary broadcast intact.
            None => ParamFormat::Binary,
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
        // Sent as i32 days since 2000-01-01 (the PostgreSQL date epoch), big-endian
        // per the PG Bind protocol (format code 1). The server adds the Julian
        // epoch itself, so the absolute Julian day number must not be sent here.
        Some(self.days().to_be_bytes().to_vec())
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
        // Sent as i64 microseconds since 2000-01-01 (the PostgreSQL timestamp
        // epoch); the server applies the Julian offset itself.
        Some(self.microseconds().to_be_bytes().to_vec())
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
        // Sent as i64 UTC microseconds since 2000-01-01 (the PostgreSQL epoch).
        Some(self.timestamp().microseconds().to_be_bytes().to_vec())
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

/// The integer type behind [`Numeric::unscaled_value`], and the only input to
/// [`pg_numeric_encode_unscaled`].
///
/// Naming it lets [`MAX_NUMERIC_GROUPS`] be *derived* from its width. If
/// `Numeric` ever widens past `i128`, the call site stops compiling (a
/// mismatched argument type) instead of silently overflowing a stack buffer,
/// and the fix — widening this alias — resizes the buffer automatically.
type UnscaledValue = i128;

/// Base-10000 groups needed for the widest [`UnscaledValue`] magnitude.
///
/// The bound is exact, not generous: an `i128` magnitude spans at most 39
/// decimal digits, so both `i128::MAX` and `i128::MIN` decompose to precisely
/// 10 groups with no slack.
const MAX_NUMERIC_GROUPS: usize = {
    let decimal_digits = UnscaledValue::MAX.ilog10() as usize + 1;
    // Each base-10000 group holds 4 decimal digits.
    decimal_digits.div_ceil(4)
};

/// Encode a whole-number (`scale == 0`) `Numeric` as PostgreSQL binary NUMERIC.
///
/// Header (i16 BE): `ndigits`, `weight`, `sign` (0x0000 pos / 0x4000 neg),
/// `dscale = 0`; then `ndigits` base-10000 groups (i16 BE, most-significant
/// first). The `weight` of the most-significant group is `ndigits - 1` (it
/// sits at base-10000 position `ndigits-1`), and `dscale` is 0 because there
/// are no fractional digits.
///
/// This handles ONLY `scale == 0` — a scaled `Numeric` binds as text instead
/// (see [`ToSqlParam for Numeric`]), so there is no scaled binary encoder.
/// The caller is responsible for only invoking this with `scale == 0`.
fn pg_numeric_encode_unscaled(unscaled: UnscaledValue) -> Vec<u8> {
    let sign_neg = unscaled < 0;
    let mut mag = unscaled.unsigned_abs();

    // Decompose the integer magnitude into base-10000 groups, least-significant
    // first. A stack array keeps the whole encode to a single allocation (the
    // returned buffer) — measurably cheaper than growing a `Vec` and reversing
    // it, and this is the hot path for every whole-number NUMERIC parameter.
    let mut groups = [0_i16; MAX_NUMERIC_GROUPS];
    let mut ngroups = 0_usize;
    while mag > 0 {
        groups[ngroups] = i16::try_from(mag % 10000)
            .expect("a base-10000 remainder is 0..=9999, which fits in i16");
        mag /= 10000;
        ngroups += 1;
    }

    let ndigits =
        i16::try_from(ngroups).expect("an i128 magnitude yields at most 10 base-10000 groups");
    let weight = if ngroups == 0 { 0 } else { ndigits - 1 };

    let mut buf = Vec::with_capacity(8 + ngroups * 2);
    buf.extend_from_slice(&ndigits.to_be_bytes());
    buf.extend_from_slice(&weight.to_be_bytes());
    buf.extend_from_slice(&(if sign_neg { 0x4000_i16 } else { 0 }).to_be_bytes());
    buf.extend_from_slice(&0_i16.to_be_bytes()); // dscale = 0 (whole number)
    // Most-significant group first.
    for g in groups[..ngroups].iter().rev() {
        buf.extend_from_slice(&g.to_be_bytes());
    }
    buf
}

impl ToSqlParam for Numeric {
    /// Binds whole numbers (`scale() == 0`) as PostgreSQL binary NUMERIC and
    /// scaled decimals (`scale() > 0`) as text.
    ///
    /// Hyper has no binary input path for a scaled NUMERIC: a binary NUMERIC
    /// whose `dscale` exceeds the parameter's resolved scale is rejected with
    /// SQLSTATE `0A000` ("cannot handle truncation when reading numerics"),
    /// and a bare `numeric` parameter OID resolves server-side to
    /// `NUMERIC(1,0)` — scale 0 — so *every* scaled value hits that check,
    /// whatever the SQL says. Correct nbase-10000 encoding does not help; the
    /// blocker is type resolution, not the bytes.
    ///
    /// Text sidesteps it, which is why [`Self::param_format`] returns
    /// [`ParamFormat::Text`] and [`Self::sql_oid`] leaves the OID unspecified
    /// for scaled values. See [`Self::sql_oid`] for what that costs.
    fn encode_param(&self) -> Option<Vec<u8>> {
        if self.scale() == 0 {
            return Some(pg_numeric_encode_unscaled(self.unscaled_value()));
        }
        // Display renders the decimal string with exactly `scale` fractional
        // digits, which is precisely Hyper's text input form for NUMERIC.
        Some(self.to_string().into_bytes())
    }

    fn param_format(&self) -> ParamFormat {
        if self.scale() == 0 {
            ParamFormat::Binary
        } else {
            ParamFormat::Text
        }
    }

    /// `NUMERIC` for whole numbers, unspecified (`0`) for scaled values.
    ///
    /// A declared `numeric` OID carries no type modifier, and Hyper resolves
    /// that to `NUMERIC(1,0)` — one digit, no fraction — so declaring it for
    /// a scaled value fails with `22003` (numeric overflow) before the text
    /// is even considered. Leaving the OID unspecified lets the server infer
    /// the type from context, which is what makes `INSERT INTO t VALUES ($1)`
    /// and `WHERE col = $1` work against a real `NUMERIC(p,s)` column.
    ///
    /// The trade-off: with no context to infer from, `SELECT $1` returns the
    /// parameter as `TEXT` rather than `NUMERIC`. That means
    /// [`Row::get::<Numeric>`](crate::Row::get) returns `None` — the column
    /// really is text — while `get::<String>` yields `"1234.56"`. Wrap it —
    /// `SELECT CAST($1 AS NUMERIC(10,2))` — when the result type matters.
    /// Two other bare-`$1` contexts reject a scaled value outright with
    /// `42601` ("unable to deduce parameter type"): `WHERE n IN ($1)` and
    /// `COALESCE($1, 0)`. And in `WHERE textcol = $1` the inference resolves
    /// `$1` to text, so the comparison is a *string* comparison: `1234.56`
    /// matches the literal `'1234.56'` but not `'1234.560'`. Whole numbers
    /// keep the concrete OID and are unaffected by all of this.
    ///
    /// # Prepared statements
    ///
    /// `sql_oid` is consulted only on the one-shot
    /// [`query_params`](crate::Connection::query_params) /
    /// [`command_params`](crate::Connection::command_params) path, which
    /// re-parses per call. A [`PreparedStatement`](crate::PreparedStatement)
    /// fixes its parameter OIDs at
    /// [`prepare_typed`](crate::Connection::prepare_typed) time, before any
    /// value exists, so **one prepared statement cannot accept both whole and
    /// scaled `NUMERIC` values**:
    ///
    /// - `prepare_typed(sql, &[oids::NUMERIC])` — whole numbers work; a
    ///   scaled value fails `22003` (numeric overflow).
    /// - `prepare_typed(sql, &[Oid::new(0)])` — scaled values work; a whole
    ///   number fails `0A000` (cannot handle truncation when reading
    ///   numerics).
    ///
    /// Use `query_params` when the scale varies across calls, or pick the OID
    /// that matches the one class you bind. `Geography` has no such split —
    /// it declares a concrete OID and always binds as text, so it works on
    /// both paths.
    fn sql_oid(&self) -> Oid {
        if self.scale() == 0 {
            oids::NUMERIC
        } else {
            Oid::new(0)
        }
    }

    fn to_sql_literal(&self) -> String {
        self.to_string()
    } // Display = decimal string
}

// =============================================================================
// Geography implementation
// =============================================================================

impl ToSqlParam for Geography {
    /// Binds as WKT text.
    ///
    /// Hyper has no PostgreSQL-binary *input* function for `geography`
    /// (binding one in binary fails with `42883`, "no pg binary input
    /// function available for type geography"), but it does accept WKT
    /// through the text path, so [`Self::param_format`] returns
    /// [`ParamFormat::Text`].
    ///
    /// # Hyper-legacy values
    ///
    /// A `Geography` read back out of Hyper is in Hyper's proprietary legacy
    /// format, which this client cannot convert to WKT
    /// ([`Geography::to_wkt`] fails, independently of parameter binding).
    /// Binding such a value sends the raw bytes, and the server rejects them
    /// with `22P02` — "invalid geography format (valid well known text is
    /// required)" — rather than storing something wrong. Check
    /// [`Geography::binary_format`] first, or construct the value with
    /// [`Geography::from_wkt`] / [`Geography::from_wkb`], whose results
    /// always bind correctly.
    ///
    /// # Empty points
    ///
    /// `Geography::from_wkt("POINT EMPTY")` re-renders as `MULTIPOINT EMPTY`
    /// — the underlying WKT writer has no representation for an empty point
    /// and widens it. That has always been true of [`Geography::to_wkt`];
    /// binding is the first path where it reaches *stored* data, so a
    /// round-trip through a parameter returns the widened type. No other
    /// geometry is affected.
    fn encode_param(&self) -> Option<Vec<u8>> {
        match self.to_wkt() {
            Ok(wkt) => Some(wkt.into_bytes()),
            // Hyper-legacy bytes: no client-side WKT is possible, so hand the
            // server bytes it will reject loudly (22P02) instead of guessing.
            Err(_) => Some(self.as_bytes().to_vec()),
        }
    }

    fn param_format(&self) -> ParamFormat {
        ParamFormat::Text
    }

    fn sql_oid(&self) -> Oid {
        oids::GEOGRAPHY
    }

    /// Renders `CAST('<wkt>' AS TABLEAU.TABGEOGRAPHY)`.
    ///
    /// Hyper-legacy bytes have no WKT rendering, and this signature has no
    /// error channel. Rather than substitute `NULL` — which would silently
    /// erase the value — the literal carries the lossily-decoded bytes, so
    /// the server rejects it with `22P02` exactly as
    /// [`Self::encode_param`] does. Single quotes are doubled in both
    /// branches, so neither can break out of the literal.
    fn to_sql_literal(&self) -> String {
        let text = match self.to_wkt() {
            Ok(wkt) => wkt,
            Err(_) => String::from_utf8_lossy(self.as_bytes()).into_owned(),
        };
        format!(
            "CAST('{}' AS TABLEAU.TABGEOGRAPHY)",
            text.replace('\'', "''")
        )
    }
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
    fn test_numeric_scale0_binds_binary_with_concrete_oid() {
        let n = Numeric::new(42, 0);
        assert_eq!(n.param_format(), ParamFormat::Binary);
        assert_eq!(n.sql_oid(), oids::NUMERIC);
    }

    #[test]
    fn test_numeric_scaled_binds_as_text() {
        // scale>0 travels as the decimal string, with the OID left
        // unspecified so the server infers a properly-scaled NUMERIC from
        // context (a declared `numeric` OID resolves to NUMERIC(1,0)).
        for (unscaled, scale, expected) in [
            (123_i128, 2_u8, "1.23"),
            (123_456, 2, "1234.56"),
            (-987_654_321, 4, "-98765.4321"),
            (5, 1, "0.5"),
            (-5, 1, "-0.5"),
            (1, 10, "0.0000000001"),
        ] {
            let n = Numeric::new(unscaled, scale);
            assert_eq!(
                n.encode_param(),
                Some(expected.as_bytes().to_vec()),
                "Numeric({unscaled}, {scale}) should encode as {expected:?}"
            );
            assert_eq!(n.param_format(), ParamFormat::Text);
            assert_eq!(n.sql_oid(), Oid::new(0));
        }
    }

    #[test]
    fn test_default_param_format_is_binary() {
        assert_eq!(42i32.param_format(), ParamFormat::Binary);
        assert_eq!("hi".param_format(), ParamFormat::Binary);
        // References and Options must forward the inner format rather than
        // fall back to the trait default. (`&&` so the blanket `&T` impl is
        // what resolves, not auto-deref to `Numeric`.)
        let scaled = Numeric::new(123, 2);
        assert_eq!((&&scaled).param_format(), ParamFormat::Text);
        assert_eq!(Some(scaled).param_format(), ParamFormat::Text);
        assert_eq!(None::<Numeric>.param_format(), ParamFormat::Binary);
    }

    #[test]
    fn test_geography_encodes_as_wkt_text() {
        let geo = Geography::from_wkt("POINT(-122.4194 37.7749)").expect("valid WKT");
        let encoded = geo.encode_param().expect("some");
        assert_eq!(
            String::from_utf8(encoded).expect("utf-8"),
            "POINT(-122.4194 37.7749)"
        );
        assert_eq!(geo.param_format(), ParamFormat::Text);
        assert_eq!(geo.sql_oid(), oids::GEOGRAPHY);
    }

    #[test]
    fn test_geography_hyper_legacy_falls_back_to_raw_bytes() {
        // Legacy bytes cannot be rendered as WKT client-side; we pass them
        // through so the server rejects them with 22P02 rather than binding
        // something wrong.
        let legacy = Geography::from_bytes(vec![0x01, 0x02, 0x03]);
        assert_eq!(legacy.encode_param(), Some(vec![0x01, 0x02, 0x03]));
        assert_eq!(legacy.param_format(), ParamFormat::Text);
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
