// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Runtime builders for `query_as!` and `query_scalar!`.

use std::marker::PhantomData;

use crate::params::{ParamFormat, ToSqlParam};
use crate::{Connection, FromRow, Result, RowValue};
use hyperdb_api_core::types::Oid;

/// A bind argument encoded eagerly so the query object owns it.
///
/// `QueryAs` / `QueryScalar` are built from borrowed macro arguments but
/// executed later, so the arguments are captured as their wire encoding
/// rather than as borrows.
#[derive(Debug)]
struct BoundParam {
    bytes: Option<Vec<u8>>,
    format: ParamFormat,
    oid: Oid,
    literal: String,
}

impl BoundParam {
    fn capture(p: &dyn ToSqlParam) -> Self {
        Self {
            bytes: p.encode_param(),
            format: p.param_format(),
            oid: p.sql_oid(),
            literal: p.to_sql_literal(),
        }
    }
}

impl ToSqlParam for BoundParam {
    fn encode_param(&self) -> Option<Vec<u8>> {
        self.bytes.clone()
    }

    fn param_format(&self) -> ParamFormat {
        self.format
    }

    fn sql_oid(&self) -> Oid {
        self.oid
    }

    fn to_sql_literal(&self) -> String {
        self.literal.clone()
    }
}

fn capture_all(params: &[&dyn ToSqlParam]) -> Vec<BoundParam> {
    params.iter().map(|p| BoundParam::capture(*p)).collect()
}

fn as_refs(params: &[BoundParam]) -> Vec<&dyn ToSqlParam> {
    params.iter().map(|p| p as &dyn ToSqlParam).collect()
}

/// A compiled, type-safe query. Created by the `query_as!` macro.
///
/// Call `.fetch_all(&conn)`, `.fetch_one(&conn)`, or `.fetch_optional(&conn)`
/// to execute it.
#[derive(Debug)]
pub struct QueryAs<T> {
    sql: String,
    params: Vec<BoundParam>,
    _phantom: PhantomData<fn() -> T>,
}

impl<T: FromRow> QueryAs<T> {
    /// Construct a new `QueryAs`. Called by the `query_as!` macro; not intended
    /// for direct use.
    ///
    /// `params` are the `$1`, `$2`, … bind arguments. Each is encoded
    /// immediately and bound by the server at execution time.
    pub fn new(sql: &str, params: &[&dyn ToSqlParam]) -> Self {
        Self {
            sql: sql.to_owned(),
            params: capture_all(params),
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
        self.fetch_rows(conn)
    }

    /// Execute the query and return exactly one row.
    ///
    /// # Errors
    ///
    /// Returns `Error::Conversion` if the query returns zero rows.
    /// Returns a `hyperdb_api::Error` on connection or SQL failure.
    pub fn fetch_one(self, conn: &Connection) -> Result<T> {
        if self.params.is_empty() {
            conn.fetch_one_as(&self.sql)
        } else {
            conn.fetch_one_as_params(&self.sql, &as_refs(&self.params))
        }
    }

    /// Execute the query and return `Some(row)` for the first row, or `None`
    /// if the query returns zero rows.
    ///
    /// # Errors
    ///
    /// Returns a `hyperdb_api::Error` on connection or SQL failure.
    pub fn fetch_optional(self, conn: &Connection) -> Result<Option<T>> {
        Ok(self.fetch_rows(conn)?.into_iter().next())
    }

    /// Runs the query, binding `$N` arguments when there are any.
    fn fetch_rows(&self, conn: &Connection) -> Result<Vec<T>> {
        if self.params.is_empty() {
            conn.fetch_all_as(&self.sql)
        } else {
            conn.fetch_all_as_params(&self.sql, &as_refs(&self.params))
        }
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
    params: Vec<BoundParam>,
    _phantom: PhantomData<fn() -> T>,
}

impl<T: RowValue> QueryScalar<T> {
    /// Construct a new `QueryScalar`. Called by the `query_scalar!` macro.
    ///
    /// `params` are the `$1`, `$2`, … bind arguments, encoded immediately.
    pub fn new(sql: &str, params: &[&dyn ToSqlParam]) -> Self {
        Self {
            sql: sql.to_owned(),
            params: capture_all(params),
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
        self.fetch_rows(conn)
            .map(|rows| rows.into_iter().map(|r| r.0).collect())
    }

    /// Execute and return exactly one scalar value.
    ///
    /// # Errors
    ///
    /// Returns `Error::Conversion` if the query returns zero rows.
    pub fn fetch_one(self, conn: &Connection) -> Result<T> {
        let rows = self.fetch_rows(conn)?;
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
        let rows = self.fetch_rows(conn)?;
        Ok(rows.into_iter().next().map(|r| r.0))
    }

    /// Runs the query, binding `$N` arguments when there are any.
    fn fetch_rows(&self, conn: &Connection) -> Result<Vec<ScalarRow<T>>> {
        if self.params.is_empty() {
            conn.fetch_all_as(&self.sql)
        } else {
            conn.fetch_all_as_params(&self.sql, &as_refs(&self.params))
        }
    }
}

/// Internal single-column `FromRow` wrapper for `QueryScalar` methods.
struct ScalarRow<T>(T);

impl<T: RowValue> FromRow for ScalarRow<T> {
    fn from_row(row: crate::RowAccessor<'_>) -> Result<Self> {
        row.position::<T>(0).map(ScalarRow)
    }
}
