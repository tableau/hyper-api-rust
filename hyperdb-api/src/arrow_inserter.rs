// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Arrow IPC stream inserter for bulk data loading.
//!
//! This module provides the [`ArrowInserter`] struct for inserting pre-formatted
//! Arrow IPC stream data into Hyper tables.
//!
//! Unlike [`Inserter`](crate::Inserter) which builds rows incrementally in `HyperBinary`
//! format, `ArrowInserter` accepts complete Arrow IPC stream data, making it ideal
//! for integration with Arrow-based data pipelines.
//!
//! # Example
//!
//! ```no_run
//! # use hyperdb_api::{ArrowInserter, Connection, TableDefinition, Result};
//! # fn example(conn: &Connection, table_def: &TableDefinition) -> Result<()> {
//! # fn get_arrow_data() -> Vec<u8> { vec![] }
//! use hyperdb_api::{ArrowInserter, Connection, TableDefinition};
//!
//! // Arrow IPC data from external source (e.g., arrow crate, Parquet reader)
//! let arrow_ipc_data: Vec<u8> = get_arrow_data();
//!
//! let mut inserter = ArrowInserter::new(&conn, &table_def)?;
//! inserter.insert_data(&arrow_ipc_data)?;
//! let rows = inserter.execute()?;
//! # Ok(())
//! # }
//! ```

use std::time::Instant;

use arrow::ipc::writer::StreamWriter;
use hyperdb_api_core::client::client::CopyInWriter;
use tracing::{debug, info};

/// Default flush threshold (16 MB) — matches `HyperBinary` Inserter.
const DEFAULT_FLUSH_THRESHOLD: usize = 16 * 1024 * 1024;

use crate::catalog::Catalog;
use crate::connection::Connection;
use crate::data_format::DataFormat;
use crate::error::{Error, Result};
use crate::table_definition::TableDefinition;

/// Tracks which insertion pathway is active, preventing unsafe mixing of
/// raw IPC methods (`insert_data`/`insert_record_batches`/`insert_raw`) with
/// the `RecordBatch`-based method (`insert_batch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertMode {
    /// Raw IPC bytes sent via `insert_data` / `insert_record_batches` / `insert_raw`.
    RawIpc,
    /// `RecordBatch` objects serialized through a `StreamWriter` via `insert_batch`.
    BatchIpc,
}

/// Inserts Arrow IPC stream data into a Hyper table.
///
/// Unlike [`Inserter`](crate::Inserter) which builds rows incrementally in `HyperBinary`
/// format, `ArrowInserter` accepts pre-formatted Arrow IPC stream data. This is useful
/// when integrating with Arrow-based data pipelines or when you already have data in
/// Arrow format.
///
/// # Arrow IPC Stream Format
///
/// Arrow IPC streams consist of:
/// 1. A schema message (describing column names and types)
/// 2. One or more record batch messages (containing the actual data)
///
/// The schema must match the target table's schema.
///
/// # Single vs Multiple Chunks
///
/// For single-chunk inserts, use [`insert_data()`](Self::insert_data) with a complete
/// Arrow IPC stream (schema + record batches).
///
/// For multiple chunks (large datasets), use:
/// 1. [`insert_data()`](Self::insert_data) for the first chunk (with schema)
/// 2. [`insert_record_batches()`](Self::insert_record_batches) for subsequent chunks (without schema)
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{ArrowInserter, Connection, CreateMode, Catalog, TableDefinition, SqlType, Result};
///
/// fn main() -> Result<()> {
///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
///
///     let table_def = TableDefinition::new("data")
///         .add_required_column("id", SqlType::int())
///         .add_nullable_column("value", SqlType::double());
///
///     Catalog::new(&conn).create_table(&table_def)?;
///
///     // Arrow IPC data from external source
///     let arrow_data: Vec<u8> = vec![]; // Your Arrow IPC stream here
///
///     let mut inserter = ArrowInserter::new(&conn, &table_def)?;
///     inserter.insert_data(&arrow_data)?;
///     let rows = inserter.execute()?;
///     println!("Inserted {} rows", rows);
///     Ok(())
/// }
/// ```
pub struct ArrowInserter<'conn> {
    connection: &'conn Connection,
    table_name: String,
    columns: Vec<String>,
    writer: Option<CopyInWriter<'conn>>,
    /// Tracks whether an Arrow schema has been sent.
    /// Sending schema twice causes an error in Hyper.
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
    /// Persistent IPC `StreamWriter` for `insert_batch()`. Streams each `RecordBatch`
    /// eagerly — the schema is written on first use, then each batch is serialized
    /// and sent immediately via `send_direct()`. The internal `Vec<u8>` buffer is
    /// drained after every write to keep memory usage bounded at `O(batch_size)`.
    batch_ipc_writer: Option<StreamWriter<Vec<u8>>>,
    /// Tracks which insertion pathway is active. `None` means no data has been
    /// sent yet. Once set, mixing pathways is an error.
    insert_mode: Option<InsertMode>,
}

impl std::fmt::Debug for ArrowInserter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArrowInserter")
            .field("table_name", &self.table_name)
            .field("columns", &self.columns)
            .field("schema_sent", &self.schema_sent)
            .field("chunk_count", &self.chunk_count)
            .field("total_bytes", &self.total_bytes)
            .finish_non_exhaustive()
    }
}

impl<'conn> ArrowInserter<'conn> {
    /// Creates a new Arrow inserter for the given table.
    ///
    /// # Arguments
    ///
    /// * `connection` - The database connection.
    /// * `table_def` - The table definition for the target table.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowInserter, Connection, TableDefinition, Result};
    /// # fn example(conn: &Connection, table_def: &TableDefinition) -> Result<()> {
    /// let inserter = ArrowInserter::new(&conn, &table_def)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] with message
    ///   `"Table definition must have at least one column"` if `table_def`
    ///   has no columns.
    /// - Returns [`Error::FeatureNotSupported`] if `connection` is using gRPC transport
    ///   (COPY is TCP-only).
    pub fn new(connection: &'conn Connection, table_def: &TableDefinition) -> Result<Self> {
        let column_count = table_def.column_count();
        if column_count == 0 {
            return Err(Error::invalid_table_definition(
                "Table definition must have at least one column",
            ));
        }

        // Fail fast: verify the connection supports COPY (TCP only).
        // The actual COPY session is started lazily on the first data write
        // to avoid locking the connection into COPY mode prematurely.
        if connection.tcp_client().is_none() {
            return Err(Error::feature_not_supported(
                "ArrowInserter requires a TCP connection. \
                 gRPC connections do not support COPY operations.",
            ));
        }

        let columns: Vec<String> = table_def.columns.iter().map(|c| c.name.clone()).collect();
        let table_name = table_def.qualified_name();

        Ok(ArrowInserter {
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
            batch_ipc_writer: None,
            insert_mode: None,
        })
    }

    /// Creates an Arrow inserter by querying the table schema from the database.
    ///
    /// This method queries the database to get the table definition automatically,
    /// which is useful when you want to insert into an existing table without
    /// manually specifying the schema.
    ///
    /// # Arguments
    ///
    /// * `connection` - The database connection.
    /// * `table_name` - The table name (can include database and schema qualifiers).
    ///
    /// # Errors
    ///
    /// Returns an error if the table doesn't exist or if the schema cannot be retrieved.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowInserter, Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let inserter = ArrowInserter::from_table(&conn, "public.my_table")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_table<T>(connection: &'conn Connection, table_name: T) -> Result<Self>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let catalog = Catalog::new(connection);
        let table_def = catalog.get_table_definition(table_name)?;
        Self::new(connection, &table_def)
    }

    /// Sets a custom flush threshold in bytes.
    ///
    /// Data is buffered until the threshold is reached, then flushed to the server.
    /// Default is 16 MB (matching `HyperBinary` Inserter).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowInserter, Connection, TableDefinition, Result};
    /// # fn example(conn: &Connection, table_def: &TableDefinition) -> Result<()> {
    /// let inserter = ArrowInserter::new(&conn, &table_def)?
    ///     .with_flush_threshold(32 * 1024 * 1024);  // 32 MB
    /// # Ok(())
    /// # }
    /// ```
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
    /// # Arguments
    ///
    /// * `arrow_ipc_data` - Complete Arrow IPC stream data (schema + record batches).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The connection fails to start COPY
    /// - Sending data fails
    /// - Schema was already sent (use `insert_record_batches()` for subsequent chunks)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowInserter, Connection, TableDefinition, Result};
    /// # fn example(conn: &Connection, table_def: &TableDefinition) -> Result<()> {
    /// # let first_arrow_ipc_stream = vec![];
    /// # let second_chunk_batches_only = vec![];
    /// let mut inserter = ArrowInserter::new(&conn, &table_def)?;
    ///
    /// // First chunk with schema
    /// inserter.insert_data(&first_arrow_ipc_stream)?;
    ///
    /// // For subsequent chunks without schema, use insert_record_batches()
    /// inserter.insert_record_batches(&second_chunk_batches_only)?;
    ///
    /// let rows = inserter.execute()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn insert_data(&mut self, arrow_ipc_data: &[u8]) -> Result<()> {
        if arrow_ipc_data.is_empty() {
            return Ok(());
        }

        if self.insert_mode == Some(InsertMode::BatchIpc) {
            return Err(Error::internal(
                "Cannot mix insert_data() with insert_batch(). \
                 Use either raw IPC methods (insert_data/insert_record_batches) \
                 or RecordBatch methods (insert_batch), not both.",
            ));
        }

        if self.schema_sent {
            return Err(Error::internal(
                "Arrow schema was already sent. Use insert_record_batches() for subsequent chunks without schema, \
                 or use insert_data() only once with the complete Arrow IPC stream.",
            ));
        }

        self.ensure_writer()?;

        if let Some(ref mut writer) = self.writer {
            writer.send_direct(arrow_ipc_data)?;
        }
        self.buffered_bytes += arrow_ipc_data.len();
        self.maybe_flush()?;

        self.insert_mode = Some(InsertMode::RawIpc);
        self.schema_sent = true;
        self.total_bytes += arrow_ipc_data.len();
        self.chunk_count += 1;

        debug!(
            target: "hyperdb_api",
            chunk = self.chunk_count,
            bytes = arrow_ipc_data.len(),
            total_bytes = self.total_bytes,
            buffered_bytes = self.buffered_bytes,
            "arrow-inserter-chunk"
        );

        Ok(())
    }

    /// Inserts Arrow record batch data without schema.
    ///
    /// Use this method for subsequent chunks after the first `insert_data()` call.
    /// The data should contain only Arrow record batch messages, **not** the schema.
    ///
    /// **Important**: Sending schema twice causes an error in Hyper. If you have
    /// multiple complete Arrow IPC streams (each with schema), you need to strip
    /// the schema from all but the first one.
    ///
    /// # Arguments
    ///
    /// * `arrow_batch_data` - Arrow record batch data (no schema message).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No schema has been sent yet (call `insert_data()` first)
    /// - Sending data fails
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowInserter, Connection, TableDefinition, Result};
    /// # fn example(conn: &Connection, table_def: &TableDefinition) -> Result<()> {
    /// # let first_chunk_with_schema = vec![];
    /// # let second_chunk_batches = vec![];
    /// # let third_chunk_batches = vec![];
    /// let mut inserter = ArrowInserter::new(&conn, &table_def)?;
    ///
    /// // First chunk must include schema
    /// inserter.insert_data(&first_chunk_with_schema)?;
    ///
    /// // Subsequent chunks are record batches only
    /// inserter.insert_record_batches(&second_chunk_batches)?;
    /// inserter.insert_record_batches(&third_chunk_batches)?;
    ///
    /// let rows = inserter.execute()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn insert_record_batches(&mut self, arrow_batch_data: &[u8]) -> Result<()> {
        if arrow_batch_data.is_empty() {
            return Ok(());
        }

        if self.insert_mode == Some(InsertMode::BatchIpc) {
            return Err(Error::internal(
                "Cannot mix insert_record_batches() with insert_batch(). \
                 Use either raw IPC methods (insert_data/insert_record_batches) \
                 or RecordBatch methods (insert_batch), not both.",
            ));
        }

        if !self.schema_sent {
            return Err(Error::internal(
                "No Arrow schema has been sent yet. Call insert_data() first with a complete \
                 Arrow IPC stream that includes the schema.",
            ));
        }

        if let Some(ref mut writer) = self.writer {
            writer.send_direct(arrow_batch_data)?;
        }
        self.buffered_bytes += arrow_batch_data.len();
        self.maybe_flush()?;

        self.total_bytes += arrow_batch_data.len();
        self.chunk_count += 1;

        debug!(
            target: "hyperdb_api",
            chunk = self.chunk_count,
            bytes = arrow_batch_data.len(),
            total_bytes = self.total_bytes,
            buffered_bytes = self.buffered_bytes,
            "arrow-inserter-batch-chunk"
        );

        Ok(())
    }

    /// Inserts raw Arrow data without schema tracking.
    ///
    /// This is a low-level method that sends data directly without checking
    /// whether schema has been sent. Use this only if you are managing schema
    /// handling yourself.
    ///
    /// For most use cases, prefer [`insert_data()`](Self::insert_data) and
    /// [`insert_record_batches()`](Self::insert_record_batches).
    ///
    /// # Arguments
    ///
    /// * `data` - Raw Arrow IPC data to send.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] if a previous `insert_batch` call already
    ///   locked the inserter into `RecordBatch` IPC mode — raw IPC and
    ///   `RecordBatch` paths cannot be mixed.
    /// - Returns [`Error::FeatureNotSupported`] / [`Error::Server`] if the lazy COPY
    ///   session fails to open.
    /// - Returns [`Error::Server`] / [`Error::Io`] if the server rejects
    ///   the data or the socket write fails.
    pub fn insert_raw(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        if self.insert_mode == Some(InsertMode::BatchIpc) {
            return Err(Error::internal(
                "Cannot mix insert_raw() with insert_batch(). \
                 Use either raw IPC methods (insert_data/insert_record_batches/insert_raw) \
                 or RecordBatch methods (insert_batch), not both.",
            ));
        }

        self.ensure_writer()?;

        if let Some(ref mut writer) = self.writer {
            writer.send_direct(data)?;
        }
        self.buffered_bytes += data.len();
        self.maybe_flush()?;

        self.total_bytes += data.len();
        self.chunk_count += 1;

        Ok(())
    }

    /// Finishes the insert operation and returns the number of rows inserted.
    ///
    /// This method completes the COPY operation and returns the row count
    /// reported by the server.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No data was sent
    /// - The COPY operation fails to complete
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowInserter, Connection, TableDefinition, Result};
    /// # fn example(conn: &Connection, table_def: &TableDefinition) -> Result<()> {
    /// # let arrow_data = vec![];
    /// let mut inserter = ArrowInserter::new(&conn, &table_def)?;
    /// inserter.insert_data(&arrow_data)?;
    /// let rows = inserter.execute()?;
    /// # Ok(())
    /// # }
    /// ```
    /// println!("Inserted {} rows", rows);
    pub fn execute(mut self) -> Result<u64> {
        // Finalize the IPC StreamWriter if insert_batch() was used.
        // `into_inner()` calls `finish()` internally (writing the EOS marker)
        // and returns the underlying buffer — a single fallible operation
        // instead of the previous finish() + into_inner() pair.
        //
        // On error, `self` is dropped, and the Drop impl cancels the COPY
        // writer, so the connection is always left in a clean state.
        if let Some(ipc) = self.batch_ipc_writer.take() {
            let buf = ipc.into_inner().map_err(|e| {
                Error::conversion(format!("Failed to finalize Arrow IPC stream: {e}"))
            })?;
            if !buf.is_empty() {
                if let Some(ref mut writer) = self.writer {
                    writer.send_direct(&buf)?;
                }
            }
        }

        if self.writer.is_none() {
            // No data was sent
            return Ok(0);
        }

        let rows = self
            .writer
            .take()
            .map(hyperdb_api_core::client::CopyInWriter::finish)
            .transpose()?
            .unwrap_or(0);

        let duration_ms = u64::try_from(self.start_time.elapsed().as_millis()).unwrap_or(u64::MAX);
        info!(
            target: "hyperdb_api",
            rows,
            chunks = self.chunk_count,
            total_bytes = self.total_bytes,
            duration_ms,
            table = %self.table_name,
            "arrow-inserter-end"
        );

        Ok(rows)
    }

    /// Cancels the insert operation.
    ///
    /// All data sent so far will be discarded.
    pub fn cancel(mut self) {
        if let Some(writer) = self.writer.take() {
            let _ = writer.cancel("Arrow insert cancelled");
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

    /// Inserts an Arrow `RecordBatch` directly, streaming it immediately.
    ///
    /// Each batch is serialized to Arrow IPC format and sent to the server right
    /// away — no accumulation in memory. The schema is written automatically on
    /// the first call; subsequent batches send only record-batch IPC messages.
    ///
    /// Memory usage is `O(batch_size)` regardless of how many batches are inserted.
    ///
    /// # Arguments
    ///
    /// * `batch` - The Arrow `RecordBatch` to insert.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowInserter, Connection, TableDefinition, SqlType, Catalog, CreateMode, Result};
    /// # fn example() -> Result<()> {
    /// # let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
    /// # let table_def = TableDefinition::new("data")
    /// #     .add_required_column("id", SqlType::int())
    /// #     .add_nullable_column("value", SqlType::double());
    /// # Catalog::new(&conn).create_table(&table_def)?;
    /// use arrow::array::{Int32Array, Float64Array};
    /// use arrow::datatypes::{Schema, Field, DataType};
    /// use arrow::record_batch::RecordBatch;
    /// use std::sync::Arc;
    ///
    /// let schema = Arc::new(Schema::new(vec![
    ///     Field::new("id", DataType::Int32, false),
    ///     Field::new("value", DataType::Float64, true),
    /// ]));
    /// let batch = RecordBatch::try_new(schema, vec![
    ///     Arc::new(Int32Array::from(vec![1, 2, 3])),
    ///     Arc::new(Float64Array::from(vec![Some(1.5), None, Some(3.5)])),
    /// ]).unwrap();
    ///
    /// let mut inserter = ArrowInserter::new(&conn, &table_def)?;
    /// inserter.insert_batch(&batch)?;
    /// let rows = inserter.execute()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] if a previous raw-IPC call locked this
    ///   inserter into the other mode — raw IPC and `RecordBatch` paths
    ///   cannot be mixed.
    /// - Returns [`Error::FeatureNotSupported`] / [`Error::Server`] if the lazy COPY
    ///   session cannot be opened.
    /// - Returns [`Error::Conversion`] wrapping the underlying Arrow IPC
    ///   writer error if the schema or batch cannot be serialized (e.g.
    ///   dictionary misalignment, encoding failure).
    /// - Returns [`Error::Server`] / [`Error::Io`] if the server rejects
    ///   the data or the socket write fails.
    ///
    /// # Panics
    ///
    /// Panics internally only if the IPC writer state is corrupted —
    /// callers cannot trigger this from the public API. The `batch_ipc_writer`
    /// is consulted via `as_mut().unwrap()` after it has just been set to
    /// `Some`, so the unwrap is unreachable.
    pub fn insert_batch(&mut self, batch: &arrow::record_batch::RecordBatch) -> Result<()> {
        if self.insert_mode == Some(InsertMode::RawIpc) {
            return Err(Error::internal(
                "Cannot mix insert_batch() with raw IPC methods. \
                 Use either RecordBatch methods (insert_batch) \
                 or raw IPC methods (insert_data/insert_record_batches/insert_raw), not both.",
            ));
        }

        self.ensure_writer()?;
        self.insert_mode = Some(InsertMode::BatchIpc);

        // Create the IPC StreamWriter on first use — this writes the schema message
        if self.batch_ipc_writer.is_none() {
            let ipc_writer = StreamWriter::try_new(Vec::new(), &batch.schema()).map_err(|e| {
                Error::conversion(format!("Failed to create Arrow IPC writer: {e}"))
            })?;
            self.batch_ipc_writer = Some(ipc_writer);

            // Drain the schema bytes that StreamWriter wrote during construction
            self.drain_ipc_buffer()?;
            self.schema_sent = true;
        }

        // Write the record batch — this appends batch IPC bytes to the internal Vec
        self.batch_ipc_writer
            .as_mut()
            .expect("IPC writer must exist")
            .write(batch)
            .map_err(|e| Error::conversion(format!("Failed to write Arrow batch: {e}")))?;

        // Drain the batch bytes and send them immediately
        self.drain_ipc_buffer()?;
        self.chunk_count += 1;

        debug!(
            target: "hyperdb_api",
            chunk = self.chunk_count,
            total_bytes = self.total_bytes,
            buffered_bytes = self.buffered_bytes,
            "arrow-inserter-batch"
        );

        Ok(())
    }

    /// Drains the internal IPC writer buffer and sends bytes via the COPY writer.
    ///
    /// # Safety contract with `StreamWriter`
    ///
    /// This method accesses the `StreamWriter`'s underlying `Vec<u8>` via
    /// `get_mut()` (part of Arrow's public API) and drains it. This is safe
    /// because `StreamWriter` writes sequentially via `Write::write_all()`
    /// and does not cache buffer offsets or positions. After draining, the
    /// Vec is empty but retains its allocation, so subsequent writes append
    /// from offset 0 without reallocation.
    fn drain_ipc_buffer(&mut self) -> Result<()> {
        let ipc = self
            .batch_ipc_writer
            .as_mut()
            .expect("IPC writer must exist");
        let buf = ipc.get_mut();
        if buf.is_empty() {
            return Ok(());
        }

        // Send the current buffer contents, then clear while preserving the
        // heap allocation so the next StreamWriter write avoids reallocation.
        let len = buf.len();
        if let Some(ref mut writer) = self.writer {
            writer.send_direct(buf)?;
        }
        buf.clear();

        self.buffered_bytes += len;
        self.total_bytes += len;
        self.maybe_flush()?;

        Ok(())
    }

    /// Inserts multiple Arrow `RecordBatch`es, streaming each one immediately.
    ///
    /// This is a convenience method that calls [`insert_batch`](Self::insert_batch)
    /// for each batch in the iterator. Memory usage stays bounded at `O(batch_size)`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowInserter, Connection, TableDefinition, Result};
    /// # fn example(conn: &Connection, table_def: &TableDefinition, batches: Vec<arrow::record_batch::RecordBatch>) -> Result<()> {
    /// let mut inserter = ArrowInserter::new(&conn, &table_def)?;
    /// inserter.insert_batches(batches.iter())?;
    /// let rows = inserter.execute()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns on the first batch that fails — see
    /// [`insert_batch`](Self::insert_batch) for the failure modes.
    pub fn insert_batches<'b>(
        &mut self,
        batches: impl IntoIterator<Item = &'b arrow::record_batch::RecordBatch>,
    ) -> Result<()> {
        for batch in batches {
            self.insert_batch(batch)?;
        }
        Ok(())
    }

    /// Ensures the COPY writer is initialized.
    fn ensure_writer(&mut self) -> Result<()> {
        if self.writer.is_none() {
            let client = self.connection.tcp_client().ok_or_else(|| {
                crate::Error::feature_not_supported(
                    "ArrowInserter requires a TCP connection. gRPC connections do not support COPY operations.",
                )
            })?;
            let columns: Vec<&str> = self
                .columns
                .iter()
                .map(std::string::String::as_str)
                .collect();
            let mut writer = client.copy_in_with_format(
                &self.table_name,
                &columns,
                DataFormat::ArrowStream.as_sql_str(),
            )?;
            // Pre-allocate buffer to avoid reallocations during bulk insert
            writer.reserve_buffer(self.flush_threshold + 1024 * 1024);
            self.writer = Some(writer);
        }
        Ok(())
    }

    /// Flushes the TCP stream if the threshold is reached.
    ///
    /// With `send_direct()`, data is written directly to TCP. This periodic
    /// flush ensures data is pushed to the server for high-latency connections.
    fn maybe_flush(&mut self) -> Result<()> {
        if self.buffered_bytes >= self.flush_threshold {
            if let Some(ref mut writer) = self.writer {
                writer.flush_stream()?;
            }
            debug!(
                target: "hyperdb_api",
                flushed_bytes = self.buffered_bytes,
                threshold = self.flush_threshold,
                "arrow-inserter-flush"
            );
            self.buffered_bytes = 0;
        }
        Ok(())
    }
}

// Implement Drop to handle cleanup if the inserter is dropped without calling execute()
impl Drop for ArrowInserter<'_> {
    fn drop(&mut self) {
        // If writer exists and we're being dropped without execute(),
        // cancel the operation to avoid leaving the connection in a bad state.
        if let Some(writer) = self.writer.take() {
            let _ = writer.cancel("Arrow inserter dropped without execute");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_format_sql_str() {
        assert_eq!(DataFormat::ArrowStream.as_sql_str(), "ARROWSTREAM");
    }
}
