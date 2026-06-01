// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `QueryAs<T>` — the runtime builder returned by `query_as!`.
//!
//! Stores `(sql, params)` and provides `fetch_all/fetch_one/fetch_optional`
//! methods that delegate to the existing `Connection::fetch_*_as` / async
//! equivalents. Full implementation in Milestone B (W3).

use std::marker::PhantomData;

use crate::{Connection, FromRow, Result};

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
