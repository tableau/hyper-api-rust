// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async high-performance bulk data inserter using the `HyperBinary` COPY
//! protocol. The async mirror of the sync [`Inserter`](crate::Inserter).
//!
//! For zero-copy Arrow IPC bulk loads, see [`AsyncArrowInserter`](crate::AsyncArrowInserter).
//!
//! # Example
//!
//! ```no_run
//! # use hyperdb_api::{AsyncConnection, AsyncInserter, CreateMode, Result, TableDefinition, SqlType};
//! # async fn run() -> Result<()> {
//! let conn = AsyncConnection::connect("localhost:7483", "data.hyper", CreateMode::CreateIfNotExists).await?;
//! let table_def = TableDefinition::new("metrics")
//!     .add_required_column("id", SqlType::int())
//!     .add_required_column("value", SqlType::double());
//! conn.execute_command(&table_def.to_create_sql(false)?).await?;
//!
//! let mut inserter = AsyncInserter::new(&conn, &table_def)?;
//! for i in 0..100_000_i32 {
//!     inserter.add_i32(i)?;
//!     inserter.add_f64(f64::from(i) * 1.5)?;
//!     inserter.end_row().await?;
//! }
//! let rows = inserter.execute().await?;
//! # let _ = rows;
//! # Ok(())
//! # }
//! ```

use std::time::Instant;

use hyperdb_api_core::client::AsyncCopyInWriter;
use hyperdb_api_core::protocol::copy;
use hyperdb_api_core::types::bytes::BytesMut;
use hyperdb_api_core::types::{Date, Interval, Numeric, OffsetTimestamp, Time, Timestamp};
use tracing::info;

use crate::async_connection::AsyncConnection;
use crate::error::{Error, Result};
use crate::inserter::InsertChunk;
use crate::table_definition::TableDefinition;

/// An async high-performance bulk data inserter.
///
/// `AsyncInserter` is the async mirror of [`Inserter`](crate::Inserter). Both
/// produce identical wire output (HyperBinary COPY) and have identical
/// throughput characteristics; the difference is whether the network I/O is
/// blocking (`Inserter`) or `async/await` (`AsyncInserter`).
///
/// # Lifetime safety
///
/// The inserter borrows the underlying [`AsyncConnection`] for `'conn`. The
/// borrow checker prevents the connection from being dropped or moved while
/// the inserter is live.
///
/// # Single-use
///
/// Like the sync `Inserter`, `AsyncInserter` is intended for one COPY session
/// per instance. After [`execute`](Self::execute) returns, the row counter is
/// zeroed so a stray second call returns `Ok(0)` rather than corrupting the
/// stream. To insert a second batch, construct a new `AsyncInserter`.
#[derive(Debug)]
pub struct AsyncInserter<'conn> {
    connection: &'conn AsyncConnection,
    table_def: TableDefinition,
    chunk: InsertChunk,
    row_count: u64,
    chunk_count: usize,
    writer: Option<AsyncCopyInWriter<'conn>>,
    start_time: Instant,
}

#[allow(
    clippy::missing_errors_doc,
    reason = "per-column add_* methods all return the same error shape \
              documented on Inserter::add_bool — repeating the same `# Errors` \
              block on 15 thin delegators adds noise without adding info"
)]
impl<'conn> AsyncInserter<'conn> {
    /// Creates a new async inserter for the given table.
    ///
    /// The underlying COPY session is started lazily on the first flush or
    /// execute, so construction is lightweight. The connection's transport
    /// is validated eagerly — a gRPC connection returns an error immediately.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] if `table_def` has zero columns.
    /// - Returns [`Error::FeatureNotSupported`] if `connection` is using gRPC transport
    ///   (COPY is TCP-only).
    pub fn new(connection: &'conn AsyncConnection, table_def: &TableDefinition) -> Result<Self> {
        if table_def.column_count() == 0 {
            return Err(Error::invalid_table_definition(
                "Table definition must have at least one column",
            ));
        }
        if connection.async_tcp_client().is_none() {
            return Err(Error::feature_not_supported(
                "AsyncInserter requires a TCP connection. \
                 gRPC connections do not support COPY operations.",
            ));
        }
        Ok(Self {
            connection,
            table_def: table_def.clone(),
            chunk: InsertChunk::from_table_definition(table_def),
            row_count: 0,
            chunk_count: 0,
            writer: None,
            start_time: Instant::now(),
        })
    }

    // -------------------------------------------------------------------
    // Per-column adders — delegate straight to the inner chunk.
    // The chunk owns column-order discipline; we just track row counts.
    // -------------------------------------------------------------------

    /// Adds a NULL value to the next column.
    pub fn add_null(&mut self) -> Result<()> {
        self.chunk.add_null()
    }
    /// Adds a boolean value.
    pub fn add_bool(&mut self, value: bool) -> Result<()> {
        self.chunk.add_bool(value)
    }
    /// Adds a 16-bit signed integer.
    pub fn add_i16(&mut self, value: i16) -> Result<()> {
        self.chunk.add_i16(value)
    }
    /// Adds a 32-bit signed integer.
    pub fn add_i32(&mut self, value: i32) -> Result<()> {
        self.chunk.add_i32(value)
    }
    /// Adds a 64-bit signed integer.
    pub fn add_i64(&mut self, value: i64) -> Result<()> {
        self.chunk.add_i64(value)
    }
    /// Adds a 32-bit float.
    pub fn add_f32(&mut self, value: f32) -> Result<()> {
        self.chunk.add_f32(value)
    }
    /// Adds a 64-bit float.
    pub fn add_f64(&mut self, value: f64) -> Result<()> {
        self.chunk.add_f64(value)
    }
    /// Adds a string (TEXT) value.
    pub fn add_str(&mut self, value: &str) -> Result<()> {
        self.chunk.add_str(value)
    }
    /// Adds a bytes (BYTEA) value.
    pub fn add_bytes(&mut self, value: &[u8]) -> Result<()> {
        self.chunk.add_bytes(value)
    }
    /// Adds a Date value.
    pub fn add_date(&mut self, value: Date) -> Result<()> {
        self.chunk.add_date(value)
    }
    /// Adds a Time value.
    pub fn add_time(&mut self, value: Time) -> Result<()> {
        self.chunk.add_time(value)
    }
    /// Adds a Timestamp value.
    pub fn add_timestamp(&mut self, value: Timestamp) -> Result<()> {
        self.chunk.add_timestamp(value)
    }
    /// Adds an OffsetTimestamp value.
    pub fn add_offset_timestamp(&mut self, value: OffsetTimestamp) -> Result<()> {
        self.chunk.add_offset_timestamp(value)
    }
    /// Adds an Interval value.
    pub fn add_interval(&mut self, value: Interval) -> Result<()> {
        self.chunk.add_interval(value)
    }
    /// Adds a `Numeric` value (NUMERIC). The encoding (small vs big) is
    /// chosen from the table definition's column precision at this position.
    ///
    /// # Errors
    ///
    /// Returns an error if the column's precision cannot be determined from
    /// the table definition (NUMERIC columns must be declared with explicit
    /// precision/scale).
    pub fn add_numeric(&mut self, value: Numeric) -> Result<()> {
        let column_index = self.chunk.column_index();
        let precision = self
            .table_def
            .columns
            .get(column_index)
            .and_then(super::table_definition::ColumnDefinition::sql_type)
            .and_then(|t| t.precision())
            .ok_or_else(|| {
                let col_name = self
                    .table_def
                    .columns
                    .get(column_index)
                    .map_or("<unknown>", |c| c.name.as_str());
                Error::conversion(format!(
                    "Cannot determine numeric precision for column '{col_name}' at index {column_index}. \
                     Ensure the column is defined with explicit SqlType including precision."
                ))
            })?;
        if precision <= Numeric::SMALL_NUMERIC_MAX_PRECISION {
            let unscaled = value.unscaled_value();
            let narrowed = i64::try_from(unscaled).map_err(|_| {
                Error::conversion(format!(
                    "Numeric value {unscaled} is out of range for i64 storage (precision {precision})"
                ))
            })?;
            self.chunk.add_i64(narrowed)
        } else {
            self.chunk.add_data128(&value.to_packed())
        }
    }

    /// Marks the end of the current row. Must be called after every full row.
    /// May trigger an automatic flush of the buffered data to the server.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] if the column count for the row doesn't match
    ///   the table definition.
    /// - Returns [`Error::Server`] / [`Error::Io`] on transport failures
    ///   during the auto-flush.
    pub async fn end_row(&mut self) -> Result<()> {
        self.chunk.end_row()?;
        self.row_count += 1;
        if self.chunk.should_flush() {
            self.flush().await?;
        }
        Ok(())
    }

    /// Sends any buffered chunk to the server. Idempotent: calling on an
    /// already-empty chunk is a no-op.
    async fn flush(&mut self) -> Result<()> {
        // Lazily start the COPY session on first flush.
        if self.writer.is_none() {
            let client = self.connection.async_tcp_client().ok_or_else(|| {
                Error::feature_not_supported(
                    "AsyncInserter requires a TCP connection. \
                     gRPC connections do not support COPY operations.",
                )
            })?;
            let columns: Vec<&str> = self
                .table_def
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            let table_name = self.table_def.qualified_name();
            self.writer = Some(client.copy_in(&table_name, &columns).await?);
        }
        if let Some(buffer) = self.chunk.take() {
            if let Some(writer) = self.writer.as_mut() {
                writer.send(&buffer).await?;
                self.chunk_count += 1;
            }
        }
        Ok(())
    }

    /// Executes the insert and commits all buffered rows.
    ///
    /// Sends any remaining buffered data and finishes the COPY operation.
    /// Returns the number of rows inserted.
    ///
    /// The inserter is single-use: calling `execute` a second time returns
    /// `Ok(0)`. Construct a new `AsyncInserter` to insert another batch.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] if there's an incomplete row (partial column).
    /// - Returns [`Error::Server`] / [`Error::Io`] if the COPY session or
    ///   transport fails.
    pub async fn execute(&mut self) -> Result<u64> {
        if self.chunk.column_index() != 0 {
            return Err(Error::invalid_table_definition(
                "Incomplete row at execute time",
            ));
        }
        if self.row_count == 0 {
            return Ok(0);
        }
        // Flush any tail data, then send the trailer and finish.
        self.flush().await?;

        let mut trailer_buf = BytesMut::with_capacity(2);
        copy::write_trailer(&mut trailer_buf);
        if let Some(writer) = self.writer.as_mut() {
            writer.send(&trailer_buf).await?;
        }

        let rows = if let Some(writer) = self.writer.take() {
            writer.finish().await?
        } else {
            0
        };

        let duration_ms = u64::try_from(self.start_time.elapsed().as_millis()).unwrap_or(u64::MAX);
        info!(
            target: "hyperdb_api",
            rows,
            chunks = self.chunk_count,
            duration_ms,
            table = %self.table_def.qualified_name(),
            "async-inserter-end"
        );

        // Reset so a stray second execute() returns Ok(0).
        self.row_count = 0;
        Ok(rows)
    }

    /// Cancels the insert and discards all buffered rows.
    ///
    /// The Drop impl on `AsyncCopyInWriter` queues a `CopyFail` on the
    /// connection so the server tears the COPY down cleanly on the next
    /// connection use.
    pub fn cancel(&mut self) {
        self.writer = None;
        self.row_count = 0;
    }
}
