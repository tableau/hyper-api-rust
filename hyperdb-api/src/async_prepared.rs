// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! High-level async prepared statements.
//!
//! Async mirror of [`PreparedStatement`](crate::PreparedStatement); see
//! that type's docs for the design rationale.

use std::sync::Arc;

use hyperdb_api_core::client::AsyncPreparedStatement as LowLevelAsyncPreparedStatement;
use hyperdb_api_core::types::Oid;

use crate::async_connection::AsyncConnection;
use crate::async_result::AsyncRowset;
use crate::async_transport::AsyncTransport;
use crate::error::{Error, Result};
use crate::params::ToSqlParam;
use crate::result::{ResultColumn, ResultSchema, Row, RowValue};

/// A handle to a server-side prepared statement (async).
///
/// Construct via [`AsyncConnection::prepare`] or
/// [`AsyncConnection::prepare_typed`]. Holding this type keeps the
/// statement allocated on the server; it is released automatically when
/// the handle is dropped (best-effort — see
/// [`hyperdb_api_core::client::AsyncPreparedStatement`] for the Drop semantics).
/// The owned variant [`AsyncPreparedStatementOwned`] also provides an
/// explicit [`close`](AsyncPreparedStatementOwned::close) method for
/// callers that want deterministic cleanup.
#[derive(Debug)]
pub struct AsyncPreparedStatement<'conn> {
    connection: &'conn AsyncConnection,
    inner: LowLevelAsyncPreparedStatement,
    schema: Arc<ResultSchema>,
}

impl<'conn> AsyncPreparedStatement<'conn> {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "signature retained for API symmetry / future fallibility; returning Result/Option keeps callers from breaking when the function later grows failure cases"
    )]
    pub(crate) fn new(
        connection: &'conn AsyncConnection,
        inner: LowLevelAsyncPreparedStatement,
    ) -> Result<Self> {
        let schema = build_schema_from_columns(inner.columns());
        Ok(Self {
            connection,
            inner,
            schema: Arc::new(schema),
        })
    }

    /// Number of parameters the statement expects.
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.inner.param_count()
    }

    /// Parameter type OIDs.
    #[must_use]
    pub fn param_types(&self) -> &[Oid] {
        self.inner.param_types()
    }

    /// Result-column schema, always available (captured at prepare time).
    #[must_use]
    pub fn schema(&self) -> &ResultSchema {
        &self.schema
    }

    /// The original SQL text.
    #[must_use]
    pub fn sql(&self) -> &str {
        self.inner.query()
    }

    /// Executes the statement and returns a streaming [`AsyncRowset`].
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transport.
    /// - Returns [`Error::Server`] if the server rejects `Bind` or
    ///   `Execute`.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub async fn query(&self, params: &[&dyn ToSqlParam]) -> Result<AsyncRowset<'conn>> {
        let encoded = encode_params(params);
        let client = async_tcp_client(self.connection)?;
        let stream = client
            .execute_prepared_streaming(
                &self.inner,
                encoded,
                crate::result::DEFAULT_BINARY_CHUNK_SIZE,
            )
            .await?;
        Ok(AsyncRowset::from_prepared(stream))
    }

    /// Executes the statement as a command and returns the affected-row
    /// count (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transport.
    /// - Returns [`Error::Server`] if the server rejects `Bind` or
    ///   `Execute`.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub async fn execute(&self, params: &[&dyn ToSqlParam]) -> Result<u64> {
        let encoded = encode_params(params);
        let client = async_tcp_client(self.connection)?;
        Ok(client
            .execute_prepared_no_result(&self.inner, encoded)
            .await?)
    }

    /// Fetches exactly one row; errors if the result is empty.
    ///
    /// # Errors
    ///
    /// - Returns the error from [`query`](Self::query).
    /// - Returns [`Error::Conversion`] with message `"Query returned no rows"`
    ///   if the result is empty.
    pub async fn fetch_one(&self, params: &[&dyn ToSqlParam]) -> Result<Row> {
        self.query(params).await?.require_first_row().await
    }

    /// Fetches at most one row; returns `None` if the result is empty.
    ///
    /// # Errors
    ///
    /// Returns the error from [`query`](Self::query); an empty result
    /// yields `Ok(None)`.
    pub async fn fetch_optional(&self, params: &[&dyn ToSqlParam]) -> Result<Option<Row>> {
        self.query(params).await?.first_row().await
    }

    /// Fetches every row into a `Vec`.
    ///
    /// # Errors
    ///
    /// Returns the error from [`query`](Self::query), or a transport
    /// error produced while draining every chunk.
    pub async fn fetch_all(&self, params: &[&dyn ToSqlParam]) -> Result<Vec<Row>> {
        self.query(params).await?.collect_rows().await
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
    pub async fn fetch_scalar<T: RowValue>(&self, params: &[&dyn ToSqlParam]) -> Result<T> {
        self.query(params).await?.require_scalar().await
    }

    /// Fetches a single scalar, allowing NULL as `None`.
    ///
    /// # Errors
    ///
    /// Returns the error from [`query`](Self::query); SQL `NULL` yields
    /// `Ok(None)`.
    pub async fn fetch_optional_scalar<T: RowValue>(
        &self,
        params: &[&dyn ToSqlParam],
    ) -> Result<Option<T>> {
        self.query(params).await?.scalar().await
    }
}

pub(crate) fn encode_params(params: &[&dyn ToSqlParam]) -> Vec<Option<Vec<u8>>> {
    params.iter().map(|p| p.encode_param()).collect()
}

// =============================================================================
// AsyncPreparedStatementOwned — lifetime-free variant
// =============================================================================

/// Owned-handle variant of [`AsyncPreparedStatement`] that holds an
/// `Arc<AsyncConnection>` instead of a borrow.
///
/// Semantics are identical to [`AsyncPreparedStatement`]. The only
/// difference is that this variant is `'static` and can therefore live
/// in structs that can't carry lifetimes — N-API classes, `tokio::spawn`
/// tasks that outlive the constructor, etc.
#[derive(Debug)]
pub struct AsyncPreparedStatementOwned {
    connection: Arc<AsyncConnection>,
    inner: LowLevelAsyncPreparedStatement,
    schema: Arc<ResultSchema>,
}

impl AsyncPreparedStatementOwned {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "signature retained for API symmetry / future fallibility; returning Result/Option keeps callers from breaking when the function later grows failure cases"
    )]
    pub(crate) fn new(
        connection: Arc<AsyncConnection>,
        inner: LowLevelAsyncPreparedStatement,
    ) -> Result<Self> {
        let schema = build_schema_from_columns(inner.columns());
        Ok(Self {
            connection,
            inner,
            schema: Arc::new(schema),
        })
    }

    /// Number of parameters the statement expects.
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.inner.param_count()
    }

    /// Parameter type OIDs.
    #[must_use]
    pub fn param_types(&self) -> &[Oid] {
        self.inner.param_types()
    }

    /// Result-column schema, captured at prepare time.
    #[must_use]
    pub fn schema(&self) -> &ResultSchema {
        &self.schema
    }

    /// Original SQL text.
    #[must_use]
    pub fn sql(&self) -> &str {
        self.inner.query()
    }

    /// Executes the statement and returns a materialized `Vec<Row>`.
    ///
    /// Unlike [`AsyncPreparedStatement::query`], the owned variant
    /// returns an owned `Vec<Row>` rather than a streaming
    /// [`AsyncRowset`]: `AsyncRowset` is itself lifetime-bound to
    /// the connection's mutex guard, which defeats the purpose of the
    /// owned wrapper. N-API callers that want streaming should fall
    /// back to the non-owned `AsyncPreparedStatement` via
    /// [`AsyncConnection::prepare`] or use the non-streaming query
    /// methods below.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transport.
    /// - Returns [`Error::Server`] if the server rejects `Bind` or
    ///   `Execute`, or raises a runtime error while streaming.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub async fn fetch_all(&self, params: &[&dyn ToSqlParam]) -> Result<Vec<Row>> {
        let encoded = encode_params(params);
        let client = async_tcp_client_arc(&self.connection)?;
        let stream = client
            .execute_prepared_streaming(
                &self.inner,
                encoded,
                crate::result::DEFAULT_BINARY_CHUNK_SIZE,
            )
            .await?;
        let rowset = AsyncRowset::from_prepared(stream);
        rowset.collect_rows().await
    }

    /// Executes the statement as a command; returns the affected-row count.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transport.
    /// - Returns [`Error::Server`] if the server rejects `Bind` or
    ///   `Execute`.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub async fn execute(&self, params: &[&dyn ToSqlParam]) -> Result<u64> {
        let encoded = encode_params(params);
        let client = async_tcp_client_arc(&self.connection)?;
        Ok(client
            .execute_prepared_no_result(&self.inner, encoded)
            .await?)
    }

    /// Fetches exactly one row; errors on empty.
    ///
    /// # Errors
    ///
    /// - Returns the error from [`fetch_all`](Self::fetch_all).
    /// - Returns [`Error::Conversion`] with message `"Query returned no rows"`
    ///   if the result is empty.
    pub async fn fetch_one(&self, params: &[&dyn ToSqlParam]) -> Result<Row> {
        self.fetch_all(params)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| crate::error::Error::conversion("Query returned no rows"))
    }

    /// Fetches at most one row; `None` on empty.
    ///
    /// # Errors
    ///
    /// Returns the error from [`fetch_all`](Self::fetch_all); an empty
    /// result yields `Ok(None)`.
    pub async fn fetch_optional(&self, params: &[&dyn ToSqlParam]) -> Result<Option<Row>> {
        Ok(self.fetch_all(params).await?.into_iter().next())
    }

    /// Fetches the first column of the first row as `T`.
    ///
    /// # Errors
    ///
    /// - Returns the error from [`fetch_one`](Self::fetch_one).
    /// - Returns [`Error::Conversion`] with message `"Scalar query returned NULL"`
    ///   if the first cell is SQL `NULL`.
    pub async fn fetch_scalar<T: RowValue>(&self, params: &[&dyn ToSqlParam]) -> Result<T> {
        let row = self.fetch_one(params).await?;
        row.get::<T>(0)
            .ok_or_else(|| crate::error::Error::conversion("Scalar query returned NULL"))
    }

    /// Fetches the first column of the first row as `Option<T>`.
    ///
    /// # Errors
    ///
    /// Returns the error from [`fetch_optional`](Self::fetch_optional);
    /// SQL `NULL` yields `Ok(None)`.
    pub async fn fetch_optional_scalar<T: RowValue>(
        &self,
        params: &[&dyn ToSqlParam],
    ) -> Result<Option<T>> {
        Ok(self
            .fetch_optional(params)
            .await?
            .and_then(|r| r.get::<T>(0)))
    }

    /// Explicitly close the statement on the server.
    ///
    /// Equivalent to dropping the struct — the inner
    /// `hyperdb_api_core::client::AsyncPreparedStatement` has its own Drop-time
    /// best-effort close.
    pub fn close(self) {
        drop(self);
    }
}

fn async_tcp_client_arc(
    connection: &Arc<AsyncConnection>,
) -> Result<&hyperdb_api_core::client::AsyncClient> {
    match connection.transport() {
        AsyncTransport::Tcp(tcp) => Ok(&tcp.client),
        AsyncTransport::Grpc(_) => Err(Error::feature_not_supported(
            "prepared statements are not supported over gRPC transport",
        )),
    }
}

pub(crate) fn async_tcp_client(
    connection: &AsyncConnection,
) -> Result<&hyperdb_api_core::client::AsyncClient> {
    match connection.transport() {
        AsyncTransport::Tcp(tcp) => Ok(&tcp.client),
        AsyncTransport::Grpc(_) => Err(Error::feature_not_supported(
            "prepared statements are not supported over gRPC transport",
        )),
    }
}

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
