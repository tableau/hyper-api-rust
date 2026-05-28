// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async Arrow IPC stream inserter for bulk data loading.
//!
//! This module provides the [`AsyncArrowInserter`] struct for inserting pre-formatted
//! Arrow IPC stream data into Hyper tables asynchronously.

use std::sync::Arc;
use std::time::Instant;

use hyperdb_api_core::client::{AsyncClient, AsyncCopyInWriter, AsyncCopyInWriterOwned};
use tracing::{debug, info};

use crate::async_connection::AsyncConnection;
use crate::data_format::DataFormat;
use crate::error::{Error, Result};
use crate::table_definition::TableDefinition;

/// Default flush threshold (16 MB) — matches `HyperBinary` Inserter.
const DEFAULT_FLUSH_THRESHOLD: usize = 16 * 1024 * 1024;

/// Async inserter for Arrow IPC stream data into a Hyper table.
///
/// This is the async version of [`ArrowInserter`](crate::ArrowInserter), designed for use
/// in tokio-based async applications.
///
/// # Ownership & Drop
///
/// You **must** call either [`execute()`](Self::execute) or [`cancel()`](Self::cancel)
/// to properly terminate the COPY session. If the inserter is dropped without
/// calling one of these, a best-effort `CopyFail` is queued and the connection
/// will self-heal on the next async operation. Data sent so far will be lost.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{AsyncArrowInserter, AsyncConnection, CreateMode, TableDefinition, SqlType, Result};
///
/// #[tokio::main]
/// async fn main() -> Result<()> {
///     let conn = AsyncConnection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists).await?;
///
///     let table_def = TableDefinition::new("data")
///         .add_required_column("id", SqlType::int())
///         .add_nullable_column("value", SqlType::double());
///
///     // Arrow IPC data from external source
///     let arrow_data: Vec<u8> = vec![]; // Your Arrow IPC stream here
///
///     let mut inserter = AsyncArrowInserter::new(&conn, &table_def)?;
///     inserter.insert_data(&arrow_data).await?;
///     let rows = inserter.execute().await?;
///     println!("Inserted {} rows", rows);
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct AsyncArrowInserter<'conn> {
    connection: &'conn AsyncConnection,
    table_name: String,
    columns: Vec<String>,
    writer: Option<AsyncCopyInWriter<'conn>>,
    /// Tracks whether an Arrow schema has been sent.
    schema_sent: bool,
    /// Total bytes sent (for logging).
    total_bytes: usize,
    /// Number of chunks sent.
    chunk_count: usize,
    /// Start time for timing the insert operation.
    start_time: Instant,
    /// Flush threshold in bytes. Data is buffered until this threshold is reached.
    flush_threshold: usize,
    /// Bytes buffered since the last flush.
    buffered_bytes: usize,
}

impl<'conn> AsyncArrowInserter<'conn> {
    /// Creates a new async Arrow inserter for the given table.
    ///
    /// The underlying COPY session is started lazily on the first data write,
    /// so construction is lightweight. However, the connection's transport is
    /// validated eagerly — using a gRPC connection will return an error
    /// immediately.
    ///
    /// # Arguments
    ///
    /// * `connection` - The async database connection (must be TCP, not gRPC).
    /// * `table_def` - The table definition for the target table.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] with message
    ///   `"Table definition must have at least one column"` if `table_def`
    ///   has no columns.
    /// - Returns [`Error::FeatureNotSupported`] if `connection` is using gRPC transport
    ///   (COPY is TCP-only).
    pub fn new(connection: &'conn AsyncConnection, table_def: &TableDefinition) -> Result<Self> {
        let column_count = table_def.column_count();
        if column_count == 0 {
            return Err(Error::invalid_table_definition(
                "Table definition must have at least one column",
            ));
        }

        // Fail fast: verify the connection supports COPY (TCP only)
        if connection.async_tcp_client().is_none() {
            return Err(Error::feature_not_supported(
                "AsyncArrowInserter requires a TCP connection. \
                 gRPC connections do not support COPY operations.",
            ));
        }

        let columns: Vec<String> = table_def.columns.iter().map(|c| c.name.clone()).collect();
        let table_name = table_def.qualified_name();

        Ok(AsyncArrowInserter {
            connection,
            table_name,
            columns,
            writer: None,
            schema_sent: false,
            total_bytes: 0,
            chunk_count: 0,
            start_time: Instant::now(),
            flush_threshold: DEFAULT_FLUSH_THRESHOLD,
            buffered_bytes: 0,
        })
    }

    /// Sets a custom flush threshold in bytes.
    ///
    /// Data is buffered until the threshold is reached, then flushed to the server.
    /// Default is 16 MB (matching `HyperBinary` Inserter).
    #[must_use]
    pub fn with_flush_threshold(mut self, threshold: usize) -> Self {
        self.flush_threshold = threshold;
        self
    }

    /// Inserts a complete Arrow IPC stream (schema + record batches).
    ///
    /// Use this method for single-chunk inserts or for the first chunk of
    /// multi-chunk inserts. The Arrow IPC stream must include the schema message
    /// followed by one or more record batch messages.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] if a schema was already sent (call
    ///   [`insert_record_batches`](Self::insert_record_batches) for
    ///   subsequent chunks instead).
    /// - Returns [`Error::FeatureNotSupported`] / [`Error::Server`] if the lazy COPY
    ///   session cannot be opened.
    /// - Returns [`Error::Server`] / [`Error::Io`] if the server rejects
    ///   the data or the socket write fails.
    pub async fn insert_data(&mut self, arrow_ipc_data: &[u8]) -> Result<()> {
        if arrow_ipc_data.is_empty() {
            return Ok(());
        }

        if self.schema_sent {
            return Err(Error::internal(
                "Arrow schema was already sent. Use insert_record_batches() for subsequent chunks without schema, \
                 or use insert_data() only once with the complete Arrow IPC stream.",
            ));
        }

        self.ensure_writer().await?;

        if let Some(ref mut writer) = self.writer {
            writer.send_direct(arrow_ipc_data).await?;
        }
        self.buffered_bytes += arrow_ipc_data.len();
        self.maybe_flush().await?;

        self.schema_sent = true;
        self.total_bytes += arrow_ipc_data.len();
        self.chunk_count += 1;

        debug!(
            target: "hyperdb_api",
            chunk = self.chunk_count,
            bytes = arrow_ipc_data.len(),
            total_bytes = self.total_bytes,
            buffered_bytes = self.buffered_bytes,
            "async-arrow-inserter-chunk"
        );

        Ok(())
    }

    /// Inserts Arrow record batch data without schema.
    ///
    /// Use this method for subsequent chunks after the first `insert_data()` call.
    /// The data should contain only Arrow record batch messages, **not** the schema.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] if no schema has been sent yet (call
    ///   [`insert_data`](Self::insert_data) first).
    /// - Returns [`Error::Server`] / [`Error::Io`] if the server rejects
    ///   the data or the socket write fails.
    pub async fn insert_record_batches(&mut self, arrow_batch_data: &[u8]) -> Result<()> {
        if arrow_batch_data.is_empty() {
            return Ok(());
        }

        if !self.schema_sent {
            return Err(Error::internal(
                "No Arrow schema has been sent yet. Call insert_data() first with a complete \
                 Arrow IPC stream that includes the schema.",
            ));
        }

        if let Some(ref mut writer) = self.writer {
            writer.send_direct(arrow_batch_data).await?;
        }
        self.buffered_bytes += arrow_batch_data.len();
        self.maybe_flush().await?;

        self.total_bytes += arrow_batch_data.len();
        self.chunk_count += 1;

        debug!(
            target: "hyperdb_api",
            chunk = self.chunk_count,
            bytes = arrow_batch_data.len(),
            total_bytes = self.total_bytes,
            buffered_bytes = self.buffered_bytes,
            "async-arrow-inserter-batch-chunk"
        );

        Ok(())
    }

    /// Inserts raw Arrow data without schema tracking.
    ///
    /// This is a low-level method that sends data directly without checking
    /// whether schema has been sent. Use this only if you are managing schema
    /// handling yourself.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] / [`Error::Server`] if the lazy COPY
    ///   session cannot be opened.
    /// - Returns [`Error::Server`] / [`Error::Io`] if the server rejects
    ///   the data or the socket write fails.
    pub async fn insert_raw(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        self.ensure_writer().await?;

        if let Some(ref mut writer) = self.writer {
            writer.send_direct(data).await?;
        }
        self.buffered_bytes += data.len();
        self.maybe_flush().await?;

        self.total_bytes += data.len();
        self.chunk_count += 1;

        Ok(())
    }

    /// Finishes the insert operation and returns the number of rows inserted.
    ///
    /// This sends any remaining buffered data to the server and completes
    /// the COPY session. Always call this (or [`cancel()`](Self::cancel))
    /// to properly terminate the session.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] or [`Error::Io`] if the `CommandComplete`
    /// round-trip fails (server rejected some buffered batch, or the socket
    /// closed mid-flush). If no data was ever written, returns `Ok(0)`.
    pub async fn execute(mut self) -> Result<u64> {
        if self.writer.is_none() {
            return Ok(0);
        }

        let rows = match self.writer.take() {
            Some(w) => w.finish().await?,
            None => 0,
        };

        let duration_ms = u64::try_from(self.start_time.elapsed().as_millis()).unwrap_or(u64::MAX);
        info!(
            target: "hyperdb_api",
            rows,
            chunks = self.chunk_count,
            total_bytes = self.total_bytes,
            duration_ms,
            table = %self.table_name,
            "async-arrow-inserter-end"
        );

        Ok(rows)
    }

    /// Cancels the insert operation.
    ///
    /// All data sent so far will be discarded.
    pub async fn cancel(mut self) {
        if let Some(writer) = self.writer.take() {
            let _ = writer.cancel("Arrow insert cancelled").await;
        }
    }

    /// Returns whether any data has been sent.
    #[must_use]
    pub fn has_data(&self) -> bool {
        self.chunk_count > 0
    }

    /// Returns the total bytes sent so far.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Returns the number of chunks sent so far.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    /// Ensures the COPY writer is initialized.
    async fn ensure_writer(&mut self) -> Result<()> {
        if self.writer.is_none() {
            let client = self.connection.async_tcp_client().ok_or_else(|| {
                crate::Error::feature_not_supported(
                    "AsyncArrowInserter requires a TCP connection. gRPC connections do not support COPY operations.",
                )
            })?;
            let columns: Vec<&str> = self
                .columns
                .iter()
                .map(std::string::String::as_str)
                .collect();
            self.writer = Some(
                client
                    .copy_in_with_format(
                        &self.table_name,
                        &columns,
                        DataFormat::ArrowStream.as_sql_str(),
                    )
                    .await?,
            );
        }
        Ok(())
    }

    /// Flushes the TCP stream if the threshold is reached.
    ///
    /// With `send_direct()`, data is written directly to TCP. This periodic
    /// flush ensures data is pushed to the server for high-latency connections.
    async fn maybe_flush(&mut self) -> Result<()> {
        if self.buffered_bytes >= self.flush_threshold {
            if let Some(ref mut writer) = self.writer {
                writer.flush_stream().await?;
            }
            debug!(
                target: "hyperdb_api",
                flushed_bytes = self.buffered_bytes,
                threshold = self.flush_threshold,
                "async-arrow-inserter-flush"
            );
            self.buffered_bytes = 0;
        }
        Ok(())
    }
}

impl Drop for AsyncArrowInserter<'_> {
    fn drop(&mut self) {
        if self.writer.is_some() {
            tracing::warn!(
                target: "hyperdb_api",
                chunks = self.chunk_count,
                total_bytes = self.total_bytes,
                table = %self.table_name,
                "AsyncArrowInserter dropped without calling execute() or cancel(). \
                 Data may be lost. The underlying AsyncCopyInWriter will \
                 attempt a best-effort cancel to restore the connection."
            );
            // Take the writer so its Drop impl runs, which queues a CopyFail
            // message via try_lock(). The next async operation on the connection
            // will drain the cancel response and restore ReadyForQuery state.
            drop(self.writer.take());
        }
    }
}

// =============================================================================
// AsyncArrowInserterOwned — lifetime-free variant
// =============================================================================

/// Owned-handle variant of [`AsyncArrowInserter`] that holds an
/// `Arc<AsyncConnection>` instead of a borrow.
///
/// Semantics are identical to [`AsyncArrowInserter`] — same
/// `HyperBinary` Arrow-stream COPY pipeline, same flush threshold,
/// same Drop-time best-effort cancel. The only difference is that
/// this variant is `'static` and can therefore live in structs that
/// can't carry lifetimes (N-API classes, `tokio::spawn` tasks that
/// outlive the constructor's stack frame, etc).
#[derive(Debug)]
pub struct AsyncArrowInserterOwned {
    #[allow(
        dead_code,
        reason = "kept alive to anchor the client's Mutex Arc for the writer's lifetime"
    )]
    connection: Arc<AsyncConnection>,
    table_name: String,
    columns: Vec<String>,
    writer: Option<AsyncCopyInWriterOwned>,
    schema_sent: bool,
    total_bytes: usize,
    chunk_count: usize,
    start_time: Instant,
    flush_threshold: usize,
    buffered_bytes: usize,
}

impl AsyncArrowInserterOwned {
    /// Creates a new owned-handle async Arrow inserter.
    ///
    /// # Arguments
    ///
    /// * `connection` - `Arc`-shared async database connection. The
    ///   Arc is cloned into the inserter and kept alive for its
    ///   lifetime, so callers can drop their own handle immediately
    ///   after construction.
    /// * `table_def` - The table definition for the target table.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] with message
    ///   `"Table definition must have at least one column"` if `table_def`
    ///   has no columns.
    /// - Returns [`Error::FeatureNotSupported`] if `connection` is using gRPC transport.
    pub fn new(connection: Arc<AsyncConnection>, table_def: &TableDefinition) -> Result<Self> {
        let column_count = table_def.column_count();
        if column_count == 0 {
            return Err(Error::invalid_table_definition(
                "Table definition must have at least one column",
            ));
        }

        if connection.async_tcp_client().is_none() {
            return Err(Error::feature_not_supported(
                "AsyncArrowInserterOwned requires a TCP connection. \
                 gRPC connections do not support COPY operations.",
            ));
        }

        let columns: Vec<String> = table_def.columns.iter().map(|c| c.name.clone()).collect();
        let table_name = table_def.qualified_name();

        Ok(AsyncArrowInserterOwned {
            connection,
            table_name,
            columns,
            writer: None,
            schema_sent: false,
            total_bytes: 0,
            chunk_count: 0,
            start_time: Instant::now(),
            flush_threshold: DEFAULT_FLUSH_THRESHOLD,
            buffered_bytes: 0,
        })
    }

    /// Sets a custom flush threshold in bytes. Default: 16 MB.
    #[must_use]
    pub fn with_flush_threshold(mut self, threshold: usize) -> Self {
        self.flush_threshold = threshold;
        self
    }

    /// Inserts a complete Arrow IPC stream (schema + record batches).
    /// Use this for single-chunk inserts or as the first call of a
    /// multi-chunk insert; subsequent chunks use
    /// [`insert_record_batches`](Self::insert_record_batches).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] if a schema was already sent.
    /// - Returns [`Error::FeatureNotSupported`] / [`Error::Server`] if the lazy COPY
    ///   session cannot be opened.
    /// - Returns [`Error::Server`] / [`Error::Io`] if the server rejects
    ///   the data or the socket write fails.
    pub async fn insert_data(&mut self, arrow_ipc_data: &[u8]) -> Result<()> {
        if arrow_ipc_data.is_empty() {
            return Ok(());
        }
        if self.schema_sent {
            return Err(Error::internal(
                "Arrow schema was already sent. Use insert_record_batches() for subsequent chunks.",
            ));
        }
        self.ensure_writer().await?;
        if let Some(ref mut w) = self.writer {
            w.send_direct(arrow_ipc_data).await?;
        }
        self.schema_sent = true;
        self.buffered_bytes += arrow_ipc_data.len();
        self.maybe_flush().await?;
        self.total_bytes += arrow_ipc_data.len();
        self.chunk_count += 1;
        Ok(())
    }

    /// Inserts Arrow record-batch bytes *without* a schema header.
    /// Must be called after [`insert_data`](Self::insert_data) or
    /// [`insert_raw`](Self::insert_raw).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] if no schema has been sent yet.
    /// - Returns [`Error::Server`] / [`Error::Io`] if the server rejects
    ///   the data or the socket write fails.
    pub async fn insert_record_batches(&mut self, arrow_batch_data: &[u8]) -> Result<()> {
        if arrow_batch_data.is_empty() {
            return Ok(());
        }
        if !self.schema_sent {
            return Err(Error::internal(
                "No Arrow schema has been sent yet. Call insert_data() first.",
            ));
        }
        if let Some(ref mut w) = self.writer {
            w.send_direct(arrow_batch_data).await?;
        }
        self.buffered_bytes += arrow_batch_data.len();
        self.maybe_flush().await?;
        self.total_bytes += arrow_batch_data.len();
        self.chunk_count += 1;
        Ok(())
    }

    /// Low-level: send raw bytes without schema tracking. The first
    /// call transitions `schema_sent` to `true`. Use this when you are
    /// managing Arrow IPC framing yourself.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] / [`Error::Server`] if the lazy COPY
    ///   session cannot be opened.
    /// - Returns [`Error::Server`] / [`Error::Io`] if the server rejects
    ///   the data or the socket write fails.
    pub async fn insert_raw(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.ensure_writer().await?;
        if let Some(ref mut w) = self.writer {
            w.send_direct(data).await?;
        }
        self.schema_sent = true;
        self.buffered_bytes += data.len();
        self.maybe_flush().await?;
        self.total_bytes += data.len();
        self.chunk_count += 1;
        Ok(())
    }

    /// Finalizes the COPY session and returns the affected row count.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] with message
    ///   `"No data was inserted before execute()"` if no COPY session was
    ///   ever opened.
    /// - Returns [`Error::Server`] / [`Error::Io`] if the `CommandComplete`
    ///   round-trip fails.
    pub async fn execute(mut self) -> Result<u64> {
        let elapsed = self.start_time.elapsed();
        info!(
            target: "hyperdb_api",
            chunks = self.chunk_count,
            total_bytes = self.total_bytes,
            elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "async-arrow-inserter-execute"
        );
        let writer = self
            .writer
            .take()
            .ok_or_else(|| Error::internal("No data was inserted before execute()"))?;
        writer.finish().await.map_err(Into::into)
    }

    /// Cancels the COPY session; any data sent so far is discarded.
    pub async fn cancel(mut self) {
        if let Some(writer) = self.writer.take() {
            let _ = writer.cancel("AsyncArrowInserterOwned::cancel").await;
        }
    }

    /// Returns `true` if any data has been inserted.
    #[must_use]
    pub fn has_data(&self) -> bool {
        self.schema_sent
    }

    /// Returns the total bytes sent.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Returns the number of chunks sent.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    async fn ensure_writer(&mut self) -> Result<()> {
        if self.writer.is_none() {
            let client: &AsyncClient = self.connection.async_tcp_client().ok_or_else(|| {
                Error::feature_not_supported(
                    "AsyncArrowInserterOwned requires a TCP connection. \
                     gRPC connections do not support COPY operations.",
                )
            })?;
            let columns: Vec<&str> = self
                .columns
                .iter()
                .map(std::string::String::as_str)
                .collect();
            self.writer = Some(
                client
                    .copy_in_arc_with_format(
                        &self.table_name,
                        &columns,
                        DataFormat::ArrowStream.as_sql_str(),
                    )
                    .await?,
            );
        }
        Ok(())
    }

    async fn maybe_flush(&mut self) -> Result<()> {
        if self.buffered_bytes >= self.flush_threshold {
            if let Some(ref mut w) = self.writer {
                w.flush_stream().await?;
            }
            debug!(
                target: "hyperdb_api",
                flushed_bytes = self.buffered_bytes,
                threshold = self.flush_threshold,
                "async-arrow-inserter-owned-flush"
            );
            self.buffered_bytes = 0;
        }
        Ok(())
    }
}

impl Drop for AsyncArrowInserterOwned {
    fn drop(&mut self) {
        if self.writer.is_some() {
            tracing::warn!(
                target: "hyperdb_api",
                chunks = self.chunk_count,
                total_bytes = self.total_bytes,
                table = %self.table_name,
                "AsyncArrowInserterOwned dropped without calling execute() or cancel(). \
                 Data may be lost."
            );
            drop(self.writer.take());
        }
    }
}
