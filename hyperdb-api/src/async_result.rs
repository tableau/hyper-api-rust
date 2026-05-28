// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async streaming result sets.
//!
//! This module is the async mirror of the sync [`Rowset`](crate::Rowset).
//! It wraps [`hyperdb_api_core::client::AsyncQueryStream`] (TCP) or
//! [`crate::ArrowRowset`] (gRPC) and presents the same row-level API as the
//! sync version — chunked iteration, schema capture, scalar collectors,
//! first-row helpers — all over `async fn`.
//!
//! Arrow-backed rowsets are fully materialized in memory already (gRPC
//! returns a complete buffer in one shot), so the Arrow path is a thin
//! wrapper around the sync [`ArrowRowset`] with no additional awaiting.

use std::sync::Arc;

use arrow::record_batch::RecordBatch;
use hyperdb_api_core::client::{AsyncQueryStream, StreamRow};
use hyperdb_api_core::types::SqlType;

use crate::arrow_result::ArrowRowset;
use crate::error::Result;
use crate::result::{ResultColumn, ResultSchema, Row, RowValue};

/// A streaming result set returned from an async query.
///
/// See [`Rowset`](crate::Rowset) for the sync equivalent and the full
/// memory-behavior contract — both use constant-memory chunked iteration.
pub struct AsyncRowset<'conn> {
    inner: AsyncRowsetInner<'conn>,
    schema_cache: Option<Arc<ResultSchema>>,
    /// Hold the prepared statement for the one-shot `query_params`
    /// path. Its Drop-time close fires only after the rowset releases
    /// its connection lock. See [`Rowset::with_statement_guard`](crate::Rowset)
    /// for the sync equivalent.
    _statement_guard: Option<hyperdb_api_core::client::AsyncPreparedStatement>,
}

impl std::fmt::Debug for AsyncRowset<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncRowset")
            .field("has_schema_cache", &self.schema_cache.is_some())
            .finish_non_exhaustive()
    }
}

enum AsyncRowsetInner<'conn> {
    /// TCP streaming result via `AsyncQueryStream`.
    Tcp(AsyncQueryStream<'conn>),
    /// Arrow-based result from gRPC. Already fully materialized; we keep
    /// it behind an `Option` so collectors that consume `self` can move
    /// the inner value out.
    Arrow(ArrowRowset),
    /// TCP streaming result from a prepared-statement execute.
    Prepared(hyperdb_api_core::client::AsyncPreparedQueryStream<'conn>),
}

impl<'conn> AsyncRowset<'conn> {
    /// Constructs a new async rowset from a TCP `AsyncQueryStream`.
    pub(crate) fn new(stream: AsyncQueryStream<'conn>) -> Self {
        Self {
            inner: AsyncRowsetInner::Tcp(stream),
            schema_cache: None,
            _statement_guard: None,
        }
    }

    /// Constructs a new async rowset from an already-materialized Arrow
    /// rowset (gRPC transport).
    pub(crate) fn from_arrow(arrow_rowset: ArrowRowset) -> Self {
        Self {
            inner: AsyncRowsetInner::Arrow(arrow_rowset),
            schema_cache: None,
            _statement_guard: None,
        }
    }

    /// Constructs a new async rowset from a prepared-statement streaming
    /// result.
    pub(crate) fn from_prepared(
        stream: hyperdb_api_core::client::AsyncPreparedQueryStream<'conn>,
    ) -> Self {
        Self {
            inner: AsyncRowsetInner::Prepared(stream),
            schema_cache: None,
            _statement_guard: None,
        }
    }

    #[expect(
        clippy::used_underscore_binding,
        reason = "underscore-prefixed parameter retained for trait-method signature compatibility"
    )]
    /// Attaches an `AsyncPreparedStatement` that should be dropped
    /// **after** this rowset is consumed. Used by the one-shot
    /// prepare+execute path inside
    /// [`crate::AsyncConnection::query_params`].
    pub(crate) fn with_statement_guard(
        mut self,
        statement: hyperdb_api_core::client::AsyncPreparedStatement,
    ) -> Self {
        self._statement_guard = Some(statement);
        self
    }

    /// Returns the schema (column metadata) for the result set.
    ///
    /// For TCP this returns `None` until the first chunk has been
    /// fetched (the `RowDescription` is the first message of the
    /// stream). For Arrow it is available immediately.
    #[must_use]
    pub fn schema(&self) -> Option<ResultSchema> {
        if let Some(ref cached) = self.schema_cache {
            return Some((**cached).clone());
        }
        self.build_schema()
    }

    fn build_schema(&self) -> Option<ResultSchema> {
        match &self.inner {
            AsyncRowsetInner::Tcp(stream) => stream.schema().map(|cols| {
                let columns = cols
                    .iter()
                    .enumerate()
                    .map(|(idx, col)| {
                        let sql_type =
                            SqlType::from_oid_and_modifier(col.type_oid().0, col.type_modifier());
                        ResultColumn::new(col.name(), sql_type, idx)
                    })
                    .collect();
                ResultSchema::from_columns(columns)
            }),
            AsyncRowsetInner::Prepared(stream) => {
                let cols = stream.schema();
                let columns = cols
                    .iter()
                    .enumerate()
                    .map(|(idx, col)| {
                        let sql_type =
                            SqlType::from_oid_and_modifier(col.type_oid().0, col.type_modifier());
                        ResultColumn::new(col.name(), sql_type, idx)
                    })
                    .collect();
                Some(ResultSchema::from_columns(columns))
            }
            AsyncRowsetInner::Arrow(arrow) => {
                let schema = arrow.schema();
                let columns = schema
                    .fields()
                    .iter()
                    .enumerate()
                    .map(|(idx, field)| {
                        ResultColumn::new(
                            field.name(),
                            crate::arrow_result::arrow_type_to_sql_type(field.data_type()),
                            idx,
                        )
                    })
                    .collect();
                Some(ResultSchema::from_columns(columns))
            }
        }
    }

    fn cached_schema_arc(&mut self) -> Option<Arc<ResultSchema>> {
        if self.schema_cache.is_none() {
            if let Some(schema) = self.build_schema() {
                self.schema_cache = Some(Arc::new(schema));
            }
        }
        self.schema_cache.clone()
    }

    /// Returns the next chunk of rows, or `None` when the stream is
    /// exhausted.
    ///
    /// # Errors
    ///
    /// - Returns [`crate::Error::Server`] if the server sends an `ErrorResponse`
    ///   while streaming the result set.
    /// - Returns [`crate::Error::Io`] on transport-level I/O failures.
    /// - Returns [`crate::Error::Conversion`] if an Arrow IPC chunk cannot be decoded.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<Row>>> {
        enum TransportChunk {
            Tcp(Vec<StreamRow>),
            Arrow(Arc<RecordBatch>),
        }

        let chunk_opt: Option<TransportChunk> = match &mut self.inner {
            AsyncRowsetInner::Tcp(stream) => stream.next_chunk().await?.map(TransportChunk::Tcp),
            AsyncRowsetInner::Arrow(arrow) => arrow
                .next_chunk()?
                .map(|chunk| TransportChunk::Arrow(Arc::new(chunk.into_batch()))),
            AsyncRowsetInner::Prepared(stream) => {
                stream.next_chunk().await?.map(TransportChunk::Tcp)
            }
        };

        let Some(chunk) = chunk_opt else {
            return Ok(None);
        };

        let schema = self.cached_schema_arc();
        let rows = match chunk {
            TransportChunk::Tcp(stream_rows) => stream_rows
                .into_iter()
                .map(|row| Row::from_tcp(row, schema.clone()))
                .collect(),
            TransportChunk::Arrow(batch) => (0..batch.num_rows())
                .map(|row_index| Row::from_arrow(Arc::clone(&batch), row_index, schema.clone()))
                .collect(),
        };
        Ok(Some(rows))
    }

    /// Collects every remaining row into a `Vec`. Consumes the rowset.
    ///
    /// # Errors
    ///
    /// Returns the first error produced by [`next_chunk`](Self::next_chunk)
    /// while draining the stream.
    pub async fn collect_rows(mut self) -> Result<Vec<Row>> {
        let mut all = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            all.extend(chunk);
        }
        Ok(all)
    }

    /// Collects the first column of every row, preserving NULL as `None`.
    ///
    /// # Errors
    ///
    /// Returns the first error produced by [`next_chunk`](Self::next_chunk).
    /// SQL `NULL` cells yield `Option::None` entries, not errors.
    pub async fn collect_column<T: RowValue>(mut self) -> Result<Vec<Option<T>>> {
        let mut values = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            for row in chunk {
                values.push(row.get::<T>(0));
            }
        }
        Ok(values)
    }

    /// Collects the first column of every row, dropping NULLs.
    ///
    /// # Errors
    ///
    /// Returns the first error produced by
    /// [`collect_column`](Self::collect_column).
    pub async fn collect_column_non_null<T: RowValue>(self) -> Result<Vec<T>> {
        Ok(self
            .collect_column::<T>()
            .await?
            .into_iter()
            .flatten()
            .collect())
    }

    /// Returns the first row of the result set, or `None` if the
    /// result is empty. Consumes the rowset.
    ///
    /// # Errors
    ///
    /// Returns the error from [`next_chunk`](Self::next_chunk). An empty
    /// result yields `Ok(None)`.
    pub async fn first_row(mut self) -> Result<Option<Row>> {
        if let Some(chunk) = self.next_chunk().await? {
            Ok(chunk.into_iter().next())
        } else {
            Ok(None)
        }
    }

    /// Returns the first row, or an error if the result set is empty.
    ///
    /// # Errors
    ///
    /// - Returns the error from [`first_row`](Self::first_row).
    /// - Returns [`crate::Error::Conversion`] with message `"Query returned no rows"`
    ///   if the result set is empty.
    pub async fn require_first_row(self) -> Result<Row> {
        self.first_row()
            .await?
            .ok_or_else(|| crate::error::Error::conversion("Query returned no rows"))
    }

    /// Returns the first column of the first row as `Option<T>`, or an
    /// error if the result set is empty.
    ///
    /// # Errors
    ///
    /// Returns the error from
    /// [`require_first_row`](Self::require_first_row). SQL `NULL` in the
    /// single cell yields `Ok(None)`.
    pub async fn scalar<T: RowValue>(self) -> Result<Option<T>> {
        Ok(self.require_first_row().await?.get(0))
    }

    /// Returns the first column of the first row as `T`, or an error
    /// if the result set is empty *or* the value is NULL.
    ///
    /// # Errors
    ///
    /// - Returns the error from [`scalar`](Self::scalar).
    /// - Returns [`crate::Error::Conversion`] with message `"Scalar query returned NULL"`
    ///   if the single cell is SQL `NULL`.
    pub async fn require_scalar<T: RowValue>(self) -> Result<T> {
        self.scalar()
            .await?
            .ok_or_else(|| crate::error::Error::conversion("Scalar query returned NULL"))
    }
}
