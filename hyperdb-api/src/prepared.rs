// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! High-level prepared statements.
//!
//! [`PreparedStatement`] wraps [`hyperdb_api_core::client::OwnedPreparedStatement`]
//! and integrates it with the rest of the hyperdb-api surface:
//!
//! - Returns [`Rowset`](crate::Rowset) from streaming executions, so
//!   row decoding, schema capture, and `Row::get::<T>()` work exactly
//!   the same way as with [`Connection::execute_query`](crate::Connection::execute_query).
//! - `execute` / `fetch_one` / `fetch_optional` / `fetch_all` /
//!   `fetch_scalar` mirror the same helpers on `Connection`.
//! - `OwnedPreparedStatement` already auto-closes on Drop at the lower
//!   layer, so this wrapper needs no additional cleanup logic.

use std::sync::Arc;

use hyperdb_api_core::client::OwnedPreparedStatement;
use hyperdb_api_core::types::Oid;

use crate::connection::Connection;
use crate::error::{Error, Result};
use crate::params::ToSqlParam;
use crate::result::{ResultColumn, ResultSchema, Row, RowValue, Rowset};
use crate::transport::Transport;

/// A handle to a server-side prepared statement.
///
/// Construct via [`Connection::prepare`] or
/// [`Connection::prepare_typed`]. Holding this type keeps the statement
/// allocated on the server; it is released automatically when the handle
/// is dropped.
///
/// # Reuse
///
/// A single `PreparedStatement` can be executed many times with different
/// parameter values — the server caches the parsed plan. This is the
/// primary reason to use prepared statements over
/// [`Connection::query_params`] for loops over user input.
#[derive(Debug)]
pub struct PreparedStatement<'conn> {
    connection: &'conn Connection,
    inner: OwnedPreparedStatement,
    schema: Arc<ResultSchema>,
}

impl<'conn> PreparedStatement<'conn> {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "signature retained for API symmetry / future fallibility; returning Result/Option keeps callers from breaking when the function later grows failure cases"
    )]
    pub(crate) fn new(
        connection: &'conn Connection,
        inner: OwnedPreparedStatement,
    ) -> Result<Self> {
        let schema = build_schema_from_columns(inner.columns());
        Ok(Self {
            connection,
            inner,
            schema: Arc::new(schema),
        })
    }

    /// Returns the number of parameters the statement expects.
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.inner.param_count()
    }

    /// Returns the parameter type OIDs (as the server inferred or the
    /// caller explicitly passed to [`Connection::prepare_typed`]).
    #[must_use]
    pub fn param_types(&self) -> &[Oid] {
        self.inner.param_types()
    }

    /// Returns the result-column schema. Always available — it was
    /// captured during the Parse/Describe at prepare time.
    #[must_use]
    pub fn schema(&self) -> &ResultSchema {
        &self.schema
    }

    /// The original SQL text.
    #[must_use]
    pub fn sql(&self) -> &str {
        self.inner.query()
    }

    /// Executes the statement and returns a streaming [`Rowset`].
    ///
    /// Memory stays bounded to one chunk regardless of result size —
    /// the prepared-statement equivalent of
    /// [`Connection::execute_query`].
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] if the underlying [`Connection`] is on
    ///   gRPC transport (prepared statements are TCP-only).
    /// - Returns [`Error::Server`] if the server rejects `Bind` or
    ///   `Execute` (type mismatch, runtime error while streaming).
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub fn query(&self, params: &[&dyn ToSqlParam]) -> Result<Rowset<'conn>> {
        let encoded = encode_params(params);
        let client = tcp_client(self.connection)?;
        let stream = client.execute_streaming(
            &self.inner,
            encoded,
            crate::result::DEFAULT_BINARY_CHUNK_SIZE,
        )?;
        Ok(Rowset::from_prepared(stream))
    }

    /// Executes the statement as a command (INSERT / UPDATE / DELETE /
    /// DDL) and returns the affected-row count.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transport.
    /// - Returns [`Error::Server`] if the server rejects `Bind` or
    ///   `Execute`.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub fn execute(&self, params: &[&dyn ToSqlParam]) -> Result<u64> {
        let encoded = encode_params(params);
        let client = tcp_client(self.connection)?;
        Ok(client.execute_no_result(&self.inner, encoded)?)
    }

    /// Fetches exactly one row; errors if the result is empty.
    ///
    /// # Errors
    ///
    /// - Returns the error from [`query`](Self::query).
    /// - Returns [`Error::Conversion`] with message `"Query returned no rows"`
    ///   if the result is empty.
    pub fn fetch_one(&self, params: &[&dyn ToSqlParam]) -> Result<Row> {
        self.query(params)?.require_first_row()
    }

    /// Fetches at most one row; returns `None` if the result is empty.
    ///
    /// # Errors
    ///
    /// Returns the error from [`query`](Self::query); an empty result
    /// yields `Ok(None)`.
    pub fn fetch_optional(&self, params: &[&dyn ToSqlParam]) -> Result<Option<Row>> {
        self.query(params)?.first_row()
    }

    /// Fetches every row into a `Vec`.
    ///
    /// # Errors
    ///
    /// Returns the error from [`query`](Self::query), or a transport error
    /// produced while draining every chunk.
    pub fn fetch_all(&self, params: &[&dyn ToSqlParam]) -> Result<Vec<Row>> {
        self.query(params)?.collect_rows()
    }

    /// Fetches a single non-NULL scalar; errors on empty / NULL.
    ///
    /// # Errors
    ///
    /// - Returns the error from [`query`](Self::query).
    /// - Returns [`Error::Conversion`] with message `"Query returned no rows"`
    ///   if the result is empty.
    /// - Returns [`Error::Conversion`] with message `"Scalar query returned NULL"`
    ///   if the first cell is SQL `NULL`.
    pub fn fetch_scalar<T: RowValue>(&self, params: &[&dyn ToSqlParam]) -> Result<T> {
        self.query(params)?.require_scalar()
    }

    /// Fetches a single scalar, allowing NULL as `None`.
    ///
    /// # Errors
    ///
    /// Returns the error from [`query`](Self::query). An empty result
    /// still errors (see [`fetch_scalar`](Self::fetch_scalar)); SQL `NULL`
    /// yields `Ok(None)`.
    pub fn fetch_optional_scalar<T: RowValue>(
        &self,
        params: &[&dyn ToSqlParam],
    ) -> Result<Option<T>> {
        self.query(params)?.scalar()
    }
}

/// Encode a slice of `&dyn ToSqlParam` into the binary-bytes form the
/// prepared-statement Bind message expects. `None` encodes SQL NULL.
pub(crate) fn encode_params(params: &[&dyn ToSqlParam]) -> Vec<Option<Vec<u8>>> {
    params.iter().map(|p| p.encode_param()).collect()
}

/// Extract the underlying sync TCP client or error with a clear message
/// if the connection is on gRPC.
pub(crate) fn tcp_client(connection: &Connection) -> Result<&hyperdb_api_core::client::Client> {
    match connection.transport() {
        Transport::Tcp(tcp) => Ok(&tcp.client),
        Transport::Grpc(_) => Err(Error::feature_not_supported(
            "prepared statements are not supported over gRPC transport",
        )),
    }
}

/// Build a `ResultSchema` from a slice of `hyperdb_api_core::client::Column`, using
/// `SqlType::from_oid_and_modifier` so NUMERIC / VARCHAR modifiers are
/// preserved.
fn build_schema_from_columns(cols: &[hyperdb_api_core::client::Column]) -> ResultSchema {
    let columns = cols
        .iter()
        .enumerate()
        .map(|(idx, col)| {
            let sql_type = hyperdb_api_core::types::SqlType::from_oid_and_modifier(
                col.type_oid().0,
                col.type_modifier(),
            );
            ResultColumn::new(col.name(), sql_type, idx)
        })
        .collect();
    ResultSchema::from_columns(columns)
}
