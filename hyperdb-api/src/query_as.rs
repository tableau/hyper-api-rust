// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Runtime builders for `query_as!` and `query_scalar!`.

use std::marker::PhantomData;

use crate::{Connection, FromRow, Result, RowValue};

/// A compiled, type-safe query. Created by the `query_as!` macro.
///
/// Call `.fetch_all(&conn)`, `.fetch_one(&conn)`, or `.fetch_optional(&conn)`
/// to execute it.
#[derive(Debug)]
pub struct QueryAs<T> {
    sql: String,
    // Bind parameters are stored as formatted strings for now. Full typed
    // parameter support (ToSqlParam) is wired in Milestone B.
    #[allow(dead_code, reason = "full parameter binding wired in Milestone B (W3)")]
    params: Vec<String>,
    _phantom: PhantomData<fn() -> T>,
}

impl<T: FromRow> QueryAs<T> {
    /// Construct a new `QueryAs`. Called by the `query_as!` macro; not intended
    /// for direct use.
    ///
    /// `params` accepts `&dyn std::fmt::Debug` so the macro can pass any bind
    /// arguments through — the actual typed binding will be tightened in W3.
    pub fn new(sql: &str, params: &[&dyn std::fmt::Debug]) -> Self {
        Self {
            sql: sql.to_owned(),
            params: params.iter().map(|p| format!("{p:?}")).collect(),
            _phantom: PhantomData,
        }
    }

    /// Execute the query and collect all rows into a `Vec<T>`.
    ///
    /// # Errors
    ///
    /// Returns a `hyperdb_api::Error` on connection failure, SQL error, or
    /// row-mapping failure.
    pub fn fetch_all(self, conn: &Connection) -> Result<Vec<T>> {
        conn.fetch_all_as(&self.sql)
    }

    /// Execute the query and return exactly one row.
    ///
    /// # Errors
    ///
    /// Returns `Error::Conversion` if the query returns zero rows.
    /// Returns a `hyperdb_api::Error` on connection or SQL failure.
    pub fn fetch_one(self, conn: &Connection) -> Result<T> {
        conn.fetch_one_as(&self.sql)
    }

    /// Execute the query and return `Some(row)` for the first row, or `None`
    /// if the query returns zero rows.
    ///
    /// # Errors
    ///
    /// Returns a `hyperdb_api::Error` on connection or SQL failure.
    pub fn fetch_optional(self, conn: &Connection) -> Result<Option<T>> {
        let rows = conn.fetch_all_as::<T>(&self.sql)?;
        Ok(rows.into_iter().next())
    }
}

/// A compiled single-column query. Created by the `query_scalar!` macro.
///
/// Returns values of a single column (e.g. `COUNT(*)`, `MAX(score)`, etc.).
/// The type `T` must implement [`RowValue`].
///
/// # Example
///
/// ```ignore
/// let count: i64 = query_scalar!(i64, "SELECT COUNT(*) FROM users").fetch_one(&conn)?;
/// let names: Vec<String> = query_scalar!(String, "SELECT name FROM users").fetch_all(&conn)?;
/// ```
#[derive(Debug)]
pub struct QueryScalar<T> {
    sql: String,
    #[allow(
        dead_code,
        reason = "typed parameter binding wired in a future milestone"
    )]
    params: Vec<String>,
    _phantom: PhantomData<fn() -> T>,
}

impl<T: RowValue> QueryScalar<T> {
    /// Construct a new `QueryScalar`. Called by the `query_scalar!` macro.
    pub fn new(sql: &str, params: &[&dyn std::fmt::Debug]) -> Self {
        Self {
            sql: sql.to_owned(),
            params: params.iter().map(|p| format!("{p:?}")).collect(),
            _phantom: PhantomData,
        }
    }

    /// Execute and return all scalar values as a `Vec<T>`.
    ///
    /// # Errors
    ///
    /// Returns a `hyperdb_api::Error` on connection failure, SQL error, or
    /// type conversion failure.
    pub fn fetch_all(self, conn: &Connection) -> Result<Vec<T>> {
        conn.fetch_all_as::<ScalarRow<T>>(&self.sql)
            .map(|rows| rows.into_iter().map(|r| r.0).collect())
    }

    /// Execute and return exactly one scalar value.
    ///
    /// # Errors
    ///
    /// Returns `Error::Conversion` if the query returns zero rows.
    pub fn fetch_one(self, conn: &Connection) -> Result<T> {
        let rows = conn.fetch_all_as::<ScalarRow<T>>(&self.sql)?;
        rows.into_iter()
            .next()
            .map(|r| r.0)
            .ok_or_else(|| crate::Error::Conversion("query_scalar!: query returned no rows".into()))
    }

    /// Execute and return `Some(value)` for the first row, or `None`.
    ///
    /// # Errors
    ///
    /// Returns a `hyperdb_api::Error` on connection or SQL failure.
    pub fn fetch_optional(self, conn: &Connection) -> Result<Option<T>> {
        let rows = conn.fetch_all_as::<ScalarRow<T>>(&self.sql)?;
        Ok(rows.into_iter().next().map(|r| r.0))
    }
}

/// Internal single-column `FromRow` wrapper for `QueryScalar` methods.
struct ScalarRow<T>(T);

impl<T: RowValue> FromRow for ScalarRow<T> {
    fn from_row(row: crate::RowAccessor<'_>) -> Result<Self> {
        row.position::<T>(0).map(ScalarRow)
    }
}
