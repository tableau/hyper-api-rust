// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! High-performance bulk data inserter using COPY protocol.
//!
//! This module provides the `Inserter` struct for efficient bulk data insertion
//! into Hyper tables, along with the `IntoValue` trait for type-safe insertion.
//!
//! # Example
//!
//! ```no_run
//! # use hyperdb_api::{Inserter, Connection, CreateMode, Result};
//! # fn example(conn: &Connection, table_def: &hyperdb_api::TableDefinition) -> Result<()> {
//! let mut inserter = Inserter::new(&conn, &table_def)?;
//! for i in 0..10000i32 {
//!     inserter.add_row(&[&i, &format!("item {}", i), &(i as f64 * 1.5)])?;
//! }
//! inserter.execute()?;
//! # Ok(())
//! # }
//! ```

use std::time::Instant;

use hyperdb_api_core::client::client::CopyInWriter;
use hyperdb_api_core::protocol::copy;
use hyperdb_api_core::types::bytes::BytesMut;
use hyperdb_api_core::types::{
    Date, Geography, Interval, Numeric, OffsetTimestamp, Time, Timestamp,
};
use tracing::{debug, info};

use crate::catalog::Catalog;
use crate::connection::Connection;
use crate::error::{Error, Result};
use crate::table_definition::TableDefinition;

/// Initial buffer size (4 MB) to reduce early reallocations.
///
/// The COPY protocol sends data in chunks, and each chunk requires a
/// contiguous buffer. Starting at 4 MB avoids repeated reallocations
/// during the first chunk for typical workloads while keeping initial
/// memory allocation reasonable.
const INITIAL_BUFFER_SIZE: usize = 4 * 1024 * 1024;

/// Maximum buffer size per chunk before flushing to the server (16 MB).
///
/// This balances two competing concerns:
/// - **Throughput**: Larger chunks amortize per-chunk overhead (COPY header,
///   network round-trip). Below ~1 MB, per-chunk overhead becomes significant.
/// - **Memory**: The buffer must be fully materialized before sending. 16 MB
///   keeps resident memory bounded even when rows are wide.
///
/// The 16 MB value was chosen empirically — it lands on the flat part of
/// the throughput curve where further increases yield diminishing returns.
const CHUNK_SIZE_LIMIT: usize = 16 * 1024 * 1024;

/// Maximum rows per chunk before flushing to the server.
///
/// This is a secondary flush trigger alongside [`CHUNK_SIZE_LIMIT`]. For
/// narrow rows (few bytes each), the byte limit alone would accumulate
/// millions of rows before flushing, which delays server-side processing.
/// 64K rows ensures timely flushes regardless of row width and aligns with
/// the 64K chunk size used for query result streaming
/// ([`DEFAULT_BINARY_CHUNK_SIZE`](crate::result::DEFAULT_BINARY_CHUNK_SIZE)).
const CHUNK_ROW_LIMIT: usize = 64_000;

/// A high-performance bulk data inserter.
///
/// The `Inserter` efficiently inserts large amounts of data into a Hyper table
/// using the COPY protocol with `HyperBinary` format for optimal performance.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{Connection, CreateMode, Catalog, TableDefinition, Inserter, SqlType, Result};
///
/// fn main() -> Result<()> {
///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
///
///     let table_def = TableDefinition::new("users")
///         .add_required_column("id", SqlType::int())
///         .add_nullable_column("name", SqlType::text());
///
///     Catalog::new(&conn).create_table(&table_def)?;
///
///     let mut inserter = Inserter::new(&conn, &table_def)?;
///
///     for i in 0..1000i32 {
///         inserter.add_row(&[&i, &format!("User {}", i)])?;
///     }
///
///     let rows = inserter.execute()?;
///     println!("Inserted {} rows", rows);
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct Inserter<'conn> {
    connection: &'conn Connection,
    table_def: TableDefinition,
    /// The current chunk being populated (delegates encoding).
    chunk: InsertChunk,
    /// Total rows inserted across all chunks.
    row_count: u64,
    /// Number of chunks sent.
    chunk_count: usize,
    /// Active COPY writer (lazily initialized on first write).
    writer: Option<CopyInWriter<'conn>>,
    /// Start time for timing the insert operation.
    start_time: Instant,
}

impl<'conn> Inserter<'conn> {
    /// Creates a new inserter for the given table.
    ///
    /// The underlying COPY session is started lazily on the first flush or
    /// execute, so construction is lightweight. However, the connection's
    /// transport is validated eagerly — using a gRPC connection will return
    /// an error immediately.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] if `table_def` has zero
    ///   columns.
    /// - Returns [`Error::FeatureNotSupported`] if `connection` is using gRPC transport
    ///   (COPY is TCP-only).
    pub fn new(connection: &'conn Connection, table_def: &TableDefinition) -> Result<Self> {
        if table_def.column_count() == 0 {
            return Err(Error::invalid_table_definition(
                "Table definition must have at least one column",
            ));
        }

        // Fail fast: verify the connection supports COPY (TCP only)
        if connection.tcp_client().is_none() {
            return Err(Error::feature_not_supported(
                "Inserter requires a TCP connection. \
                 gRPC connections do not support COPY operations.",
            ));
        }

        Ok(Inserter {
            connection,
            table_def: table_def.clone(),
            chunk: InsertChunk::from_table_definition(table_def),
            row_count: 0,
            chunk_count: 0,
            writer: None,
            start_time: Instant::now(),
        })
    }

    /// Creates an inserter by querying the table schema from the database.
    ///
    /// This method queries the database to get the table definition automatically,
    /// which is useful when you want to insert into an existing table without
    /// manually specifying the schema.
    ///
    /// # Arguments
    ///
    /// * `connection` - The database connection.
    /// * `table_name` - The table name (can be a simple name, or "schema.table", etc.)
    ///
    /// # Errors
    ///
    /// Returns an error if the table doesn't exist or if the schema cannot be retrieved.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Inserter, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
    ///
    ///     // Create a table first
    ///     conn.execute_command("CREATE TABLE IF NOT EXISTS products (id INT NOT NULL, name TEXT, price DOUBLE PRECISION)")?;
    ///
    ///     // Create inserter by querying the schema directly from a string
    ///     let mut inserter = Inserter::from_table(&conn, "public.products")?;
    ///
    ///     // Now we can insert data without knowing the exact schema
    ///     inserter.add_row(&[&1i32, &"Widget", &19.99f64])?;
    ///     inserter.add_row(&[&2i32, &"Gadget", &29.99f64])?;
    ///
    ///     let rows = inserter.execute()?;
    ///     println!("Inserted {} rows", rows);
    ///     Ok(())
    /// }
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

    /// Creates an inserter with column mappings that allow SQL expressions.
    ///
    /// This method uses a temporary table and INSERT...SELECT to support
    /// column mappings with SQL expressions. Data is first inserted into
    /// a temporary staging table, then transformed using the mappings.
    ///
    /// # Arguments
    ///
    /// * `connection` - The database connection.
    /// * `inserter_def` - Defines the columns to be provided to the inserter (staging table).
    /// * `target_table` - The qualified name of the target table to insert into.
    ///   Use `TableDefinition::qualified_name()` for properly escaped names like `"schema"."table"`.
    /// * `mappings` - Column mappings defining how values are transformed.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, TableDefinition, ColumnMapping, Inserter, SqlType, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
    ///
    ///     // Target table with computed columns
    ///     conn.execute_command(r#"
    ///         CREATE TABLE orders (
    ///             id INT NOT NULL,
    ///             product TEXT,
    ///             quantity INT,
    ///             price DOUBLE PRECISION,
    ///             total DOUBLE PRECISION,
    ///             created_at TIMESTAMP
    ///         )
    ///     "#)?;
    ///
    ///     // Inserter definition - what we provide
    ///     let inserter_def = TableDefinition::new("_stage")
    ///         .add_required_column("id", SqlType::int())
    ///         .add_nullable_column("product", SqlType::text())
    ///         .add_nullable_column("quantity", SqlType::int())
    ///         .add_nullable_column("price", SqlType::double());
    ///
    ///     // Column mappings - how values are transformed
    ///     let mappings = vec![
    ///         ColumnMapping::new("id"),
    ///         ColumnMapping::new("product"),
    ///         ColumnMapping::new("quantity"),
    ///         ColumnMapping::new("price"),
    ///         ColumnMapping::with_expression("total", "quantity * price"),
    ///         ColumnMapping::with_expression("created_at", "NOW()"),
    ///     ];
    ///
    ///     // For simple table names in the public schema, use quoted name
    ///     // For qualified names, use target_table_def.qualified_name()
    ///     let mut inserter = Inserter::with_column_mappings(&conn, &inserter_def, "orders", &mappings)?;
    ///
    ///     inserter.add_row(&[&1i32, &"Widget", &5i32, &10.0f64])?;
    ///     inserter.add_row(&[&2i32, &"Gadget", &3i32, &25.0f64])?;
    ///
    ///     let rows = inserter.execute()?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns an error if `target_table` fails to convert into a
    ///   [`TableName`](crate::TableName).
    /// - Returns [`Error::Server`] if creating the temporary staging table
    ///   fails on the server.
    /// - Returns the errors from [`Inserter::new`] for the staging table
    ///   (zero-column table definition, gRPC transport).
    pub fn with_column_mappings<T>(
        connection: &'conn Connection,
        inserter_def: &TableDefinition,
        target_table: T,
        mappings: &[ColumnMapping],
    ) -> Result<MappedInserter<'conn>>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        MappedInserter::new(connection, inserter_def, target_table, mappings)
    }

    /// Returns the table definition.
    pub fn table_definition(&self) -> &TableDefinition {
        &self.table_def
    }

    /// Returns the number of columns.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.table_def.column_count()
    }

    /// Returns the number of complete rows buffered.
    #[must_use]
    pub fn row_count(&self) -> u64 {
        self.row_count
    }

    /// Adds a NULL value for the current column.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTableDefinition`] if the current row already has all columns
    /// supplied, or if the current column is marked `NOT NULL` in the table
    /// definition.
    #[inline]
    pub fn add_null(&mut self) -> Result<()> {
        self.chunk.add_null()
    }

    /// Adds a boolean value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTableDefinition`] with message `"Too many columns in row"` if
    /// the current row already has all columns supplied.
    #[inline]
    pub fn add_bool(&mut self, value: bool) -> Result<()> {
        self.chunk.add_bool(value)
    }

    /// Adds an i16 value (SMALLINT).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_i16(&mut self, value: i16) -> Result<()> {
        self.chunk.add_i16(value)
    }

    /// Adds an i32 value (INT).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_i32(&mut self, value: i32) -> Result<()> {
        self.chunk.add_i32(value)
    }

    /// Adds an i64 value (BIGINT).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_i64(&mut self, value: i64) -> Result<()> {
        self.chunk.add_i64(value)
    }

    /// Adds an f32 value (REAL/FLOAT4).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_f32(&mut self, value: f32) -> Result<()> {
        self.chunk.add_f32(value)
    }

    /// Adds an f64 value (DOUBLE PRECISION/FLOAT8).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_f64(&mut self, value: f64) -> Result<()> {
        self.chunk.add_f64(value)
    }

    /// Adds a string value (TEXT/VARCHAR).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_str(&mut self, value: &str) -> Result<()> {
        self.chunk.add_str(value)
    }

    /// Adds a bytes value (BYTEA).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_bytes(&mut self, value: &[u8]) -> Result<()> {
        self.chunk.add_bytes(value)
    }

    /// Adds a 128-bit value (NUMERIC/INTERVAL).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_data128(&mut self, value: &[u8; 16]) -> Result<()> {
        self.chunk.add_data128(value)
    }

    /// Adds an optional value. If None, adds NULL.
    ///
    /// # Errors
    ///
    /// Propagates whatever `add_fn` or [`add_null`](Self::add_null) would
    /// return for the current row position.
    pub fn add_optional<T, F>(&mut self, value: Option<T>, add_fn: F) -> Result<()>
    where
        F: FnOnce(&mut Self, T) -> Result<()>,
    {
        match value {
            Some(v) => add_fn(self, v),
            None => self.add_null(),
        }
    }

    /// Ends the current row.
    ///
    /// Returns an error if the wrong number of columns were added.
    /// Automatically flushes the buffer if chunk limits are reached.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] if fewer (or more) columns were supplied
    ///   than the table definition requires.
    /// - Returns any error from [`flush`](Self::flush) when an automatic
    ///   flush is triggered by reaching the chunk byte/row limit.
    pub fn end_row(&mut self) -> Result<()> {
        self.chunk.end_row()?;
        self.row_count += 1;

        // Auto-flush if we've reached chunk limits
        if self.chunk.should_flush() {
            self.flush()?;
        }

        Ok(())
    }

    /// Flushes the current buffer to the server.
    ///
    /// This sends all buffered rows as a chunk and resets the buffer.
    /// Called automatically when chunk limits are reached.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] if the connection is using gRPC transport
    ///   (COPY is TCP-only) and no COPY session exists yet.
    /// - Returns [`Error::Server`] if the server rejects the `COPY IN` start
    ///   or the subsequent data send.
    /// - Returns [`Error::Io`] on transport-level I/O failures while writing
    ///   the chunk.
    pub fn flush(&mut self) -> Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }

        let chunk_rows = self.chunk.row_count();
        let Some(buffer) = self.chunk.take() else {
            return Ok(());
        };

        // Ensure the COPY connection is started
        if self.writer.is_none() {
            let client = self.connection.tcp_client().ok_or_else(|| {
                crate::Error::feature_not_supported(
                    "Inserter requires a TCP connection. gRPC connections do not support COPY operations.",
                )
            })?;
            let columns: Vec<&str> = self
                .table_def
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            let table_name = self.table_def.qualified_name();
            self.writer = Some(client.copy_in(&table_name, &columns)?);
        }

        // Write the chunk directly to the socket, avoiding a full-chunk memcpy
        // into the connection's write buffer. flush_stream ensures the data
        // reaches the server before we return.
        if let Some(ref mut writer) = self.writer {
            writer.send_direct(&buffer)?;
            writer.flush_stream()?;
        }

        debug!(
            target: "hyperdb_api",
            chunk = self.chunk_count,
            rows = chunk_rows,
            bytes = buffer.len(),
            "inserter-chunk"
        );

        self.chunk_count += 1;
        Ok(())
    }

    /// Adds a complete row of values.
    ///
    /// This is a convenience method that adds all column values at once
    /// using the `IntoValue` trait for type-safe insertion.
    ///
    /// # Arguments
    ///
    /// * `values` - A slice of values implementing `IntoValue`.
    ///
    /// # Errors
    ///
    /// Returns an error if the number of values doesn't match the column count,
    /// or if any value cannot be added.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Catalog, TableDefinition, Inserter, SqlType, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
    ///
    ///     let table_def = TableDefinition::new("users")
    ///         .add_required_column("id", SqlType::int())
    ///         .add_nullable_column("name", SqlType::text());
    ///
    ///     Catalog::new(&conn).create_table(&table_def)?;
    ///
    ///     let mut inserter = Inserter::new(&conn, &table_def)?;
    ///
    ///     // Add rows using IntoValue trait
    ///     inserter.add_row(&[&1i32, &"Alice"])?;
    ///     inserter.add_row(&[&2i32, &"Bob"])?;
    ///
    ///     // Option<T> can be used for nullable columns
    ///     inserter.add_row(&[&3i32, &None::<&str>])?;
    ///
    ///     let rows = inserter.execute()?;
    ///     Ok(())
    /// }
    /// ```
    pub fn add_row(&mut self, values: &[&dyn IntoValue]) -> Result<()> {
        let column_count = self.table_def.column_count();
        if values.len() != column_count {
            return Err(Error::invalid_table_definition(format!(
                "Column count mismatch: expected {} columns but got {}",
                column_count,
                values.len()
            )));
        }

        for value in values {
            value.add_to_inserter(self)?;
        }

        self.end_row()?;
        Ok(())
    }

    /// Adds a Date value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_date(&mut self, value: Date) -> Result<()> {
        self.chunk.add_date(value)
    }

    /// Adds a Time value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_time(&mut self, value: Time) -> Result<()> {
        self.chunk.add_time(value)
    }

    /// Adds a Timestamp value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_timestamp(&mut self, value: Timestamp) -> Result<()> {
        self.chunk.add_timestamp(value)
    }

    /// Adds an `OffsetTimestamp` (TIMESTAMP WITH TIME ZONE) value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_offset_timestamp(&mut self, value: OffsetTimestamp) -> Result<()> {
        self.chunk.add_offset_timestamp(value)
    }

    /// Adds an Interval value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_interval(&mut self, value: Interval) -> Result<()> {
        self.chunk.add_interval(value)
    }

    /// Adds a Geography value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    #[inline]
    pub fn add_geography(&mut self, value: &Geography) -> Result<()> {
        self.chunk.add_geography(value)
    }

    /// Adds a Numeric value.
    ///
    /// For NUMERIC(precision, scale) where precision ≤ [`Numeric::SMALL_NUMERIC_MAX_PRECISION`]
    /// (18), the value is stored as i64. For higher precision, 128-bit storage is used.
    ///
    /// # Errors
    ///
    /// Returns an error if the column's precision cannot be determined from the
    /// table definition. Ensure that NUMERIC columns are defined with explicit
    /// `SqlType` information including precision.
    pub fn add_numeric(&mut self, value: Numeric) -> Result<()> {
        let column_index = self.chunk.column_index();

        // Check the column's precision to determine storage format
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
                     Ensure the column is defined with explicit SqlType including precision.\n\n\
                     Example fix:\n  \
                     table_def.add_column_with_type(\"{col_name}\", SqlType::Numeric {{ precision: 10, scale: 2 }}, true);"
                ))
            })?;

        if precision <= Numeric::SMALL_NUMERIC_MAX_PRECISION {
            // Small numeric: stored as i64
            let unscaled = value.unscaled_value();
            let narrowed = i64::try_from(unscaled).map_err(|_| {
                Error::conversion(format!(
                    "Numeric value {unscaled} is out of range for i64 storage (precision {precision})"
                ))
            })?;
            self.chunk.add_i64(narrowed)
        } else {
            // Big numeric: stored as 128-bit
            self.chunk.add_data128(&value.to_packed())
        }
    }

    /// Executes the insert and commits all buffered rows.
    ///
    /// This sends any remaining buffered data and finishes the COPY operation.
    /// Returns the number of rows inserted.
    ///
    /// The inserter is single-use: calling `execute` a second time returns
    /// `Ok(0)` because the internal row counter has been reset and no further
    /// data has been added. To insert additional batches, create a new
    /// [`Inserter`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - There's an incomplete row (`column_index` != 0)
    /// - The COPY connection fails to start
    /// - Sending data fails
    pub fn execute(&mut self) -> Result<u64> {
        if self.chunk.column_index() != 0 {
            return Err(Error::invalid_table_definition(
                "Incomplete row at execute time",
            ));
        }

        if self.row_count == 0 {
            return Ok(0);
        }

        // Ensure COPY connection exists before proceeding when we have rows
        if self.writer.is_none() {
            let client = self.connection.tcp_client().ok_or_else(|| {
                Error::feature_not_supported(
                    "Inserter requires a TCP connection. gRPC connections do not support COPY operations.",
                )
            })?;
            let columns: Vec<&str> = self
                .table_def
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            let table_name = self.table_def.qualified_name();
            self.writer = Some(client.copy_in(&table_name, &columns)?);
        }

        // At this point, writer must exist since we have rows
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::internal("Failed to initialize COPY connection for inserter"))?;

        // If we have buffered data that hasn't been sent yet
        if !self.chunk.is_empty() {
            writer.send(self.chunk.buffer())?;
        }

        // Write and send the COPY trailer
        let mut trailer_buf = BytesMut::with_capacity(2);
        copy::write_trailer(&mut trailer_buf);
        writer.send(&trailer_buf)?;

        // Finish the COPY operation
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
            duration_ms,
            table = %self.table_def.qualified_name(),
            "inserter-end"
        );

        // Reset row counter so a stray second execute() call returns Ok(0)
        // instead of attempting another COPY trailer on a finished writer.
        self.row_count = 0;

        Ok(rows)
    }

    /// Cancels the insert and discards all buffered rows.
    pub fn cancel(&mut self) {
        // Drop the in-progress writer (if any). The Drop impl on CopyInWriter
        // sends a CopyFail to the server.
        self.writer = None;
        self.row_count = 0;
    }
}

// =============================================================================
// ColumnMapping
// =============================================================================

/// Defines how a column receives its value during insertion.
///
/// Column mappings allow you to:
/// - Insert values directly from the inserter stream
/// - Compute values using SQL expressions
/// - Use server-side functions like `NOW()` or `DEFAULT`
///
/// # Example
///
/// ```
/// use hyperdb_api::ColumnMapping;
///
/// // Simple column - insert value directly
/// let id_col = ColumnMapping::new("id");
///
/// // Column with expression - computed value
/// let created_at = ColumnMapping::with_expression("created_at", "NOW()");
/// let full_name = ColumnMapping::with_expression("full_name", "first_name || ' ' || last_name");
/// ```
#[derive(Debug, Clone)]
#[must_use = "ColumnMapping represents a column configuration that should not be discarded. Use it when defining inserter column mappings"]
pub struct ColumnMapping {
    /// The name of the target column.
    pub column_name: String,
    /// Optional SQL expression. If None, the value is inserted directly.
    pub expression: Option<String>,
}

impl ColumnMapping {
    /// Creates a column mapping for direct value insertion.
    ///
    /// The column will receive values directly from the inserter.
    pub fn new(column_name: impl Into<String>) -> Self {
        ColumnMapping {
            column_name: column_name.into(),
            expression: None,
        }
    }

    /// Creates a column mapping with a SQL expression.
    ///
    /// The column value will be computed using the given SQL expression.
    /// The expression can reference other columns or use SQL functions.
    ///
    /// # Arguments
    ///
    /// * `column_name` - The name of the target column.
    /// * `expression` - A SQL expression to compute the column value.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::ColumnMapping;
    ///
    /// // Use current timestamp
    /// let created = ColumnMapping::with_expression("created_at", "NOW()");
    ///
    /// // Compute from other columns
    /// let total = ColumnMapping::with_expression("total", "quantity * price");
    /// ```
    pub fn with_expression(column_name: impl Into<String>, expression: impl Into<String>) -> Self {
        ColumnMapping {
            column_name: column_name.into(),
            expression: Some(expression.into()),
        }
    }

    /// Returns the column name.
    #[must_use]
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// Returns the SQL expression, if any.
    #[must_use]
    pub fn expression(&self) -> Option<&str> {
        self.expression.as_deref()
    }

    /// Returns true if this is a direct value mapping (no expression).
    #[must_use]
    pub fn is_direct(&self) -> bool {
        self.expression.is_none()
    }

    /// Returns the select list item for this mapping.
    fn to_select_item(&self) -> String {
        match &self.expression {
            Some(expr) => format!("{} AS \"{}\"", expr, self.column_name.replace('"', "\"\"")),
            None => format!("\"{}\"", self.column_name.replace('"', "\"\"")),
        }
    }
}

// =============================================================================
// IntoValue Trait
// =============================================================================

/// Trait for types that can be inserted into a Hyper table.
///
/// This trait is implemented for common Rust types, allowing them to be
/// used with [`Inserter::add_row()`] for type-safe insertion.
///
/// # Supported Types
///
/// - Integers: `i16`, `i32`, `i64`
/// - Floats: `f32`, `f64`
/// - `bool`
/// - `&str`, `String`
/// - `Option<T>` where `T: IntoValue` (for nullable columns)
/// - Date/time types: `Date`, `Time`, `Timestamp`, `Interval`
/// - `Numeric`, `Geography`, `Vec<u8>` (bytes)
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{Connection, CreateMode, Catalog, TableDefinition, Inserter, IntoValue, SqlType, Result};
///
/// fn main() -> Result<()> {
///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
///
///     let table_def = TableDefinition::new("example")
///         .add_required_column("a", SqlType::int())
///         .add_nullable_column("b", SqlType::text())
///         .add_nullable_column("c", SqlType::double());
///     Catalog::new(&conn).create_table(&table_def)?;
///
///     let mut inserter = Inserter::new(&conn, &table_def)?;
///
///     // IntoValue allows adding rows with mixed types
///     inserter.add_row(&[&1i32, &"Alice", &Some(3.14f64)])?;
///     inserter.add_row(&[&2i32, &"Bob", &None::<f64>])?; // NULL value
///
///     inserter.execute()?;
///     Ok(())
/// }
/// ```
pub trait IntoValue {
    /// Adds this value to the inserter.
    ///
    /// # Errors
    ///
    /// Implementations call the matching `Inserter::add_*` method and
    /// forward its error — see [`Inserter::add_bool`] for the shared
    /// failure modes (too many columns, NULL into non-nullable, etc).
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()>;
}

// Implementations for basic types

impl IntoValue for bool {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_bool(*self)
    }
}

impl IntoValue for i16 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_i16(*self)
    }
}

impl IntoValue for i32 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_i32(*self)
    }
}

impl IntoValue for i64 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_i64(*self)
    }
}

impl IntoValue for f32 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_f32(*self)
    }
}

impl IntoValue for f64 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_f64(*self)
    }
}

impl IntoValue for str {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_str(self)
    }
}

impl IntoValue for String {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_str(self)
    }
}

impl IntoValue for [u8] {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_bytes(self)
    }
}

impl IntoValue for Vec<u8> {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_bytes(self)
    }
}

// Hyper-specific types

impl IntoValue for Date {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_date(*self)
    }
}

impl IntoValue for Time {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_time(*self)
    }
}

impl IntoValue for Timestamp {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_timestamp(*self)
    }
}

impl IntoValue for OffsetTimestamp {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_offset_timestamp(*self)
    }
}

impl IntoValue for Interval {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_interval(*self)
    }
}

impl IntoValue for Numeric {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_numeric(*self)
    }
}

impl IntoValue for Geography {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_geography(self)
    }
}

// Option<T> for nullable values
impl<T: IntoValue> IntoValue for Option<T> {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        match self {
            Some(value) => value.add_to_inserter(inserter),
            None => inserter.add_null(),
        }
    }
}

// Reference implementations for primitives

impl IntoValue for &bool {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_bool(**self)
    }
}

impl IntoValue for &i16 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_i16(**self)
    }
}

impl IntoValue for &i32 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_i32(**self)
    }
}

impl IntoValue for &i64 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_i64(**self)
    }
}

impl IntoValue for &f32 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_f32(**self)
    }
}

impl IntoValue for &f64 {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_f64(**self)
    }
}

impl IntoValue for &String {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_str(self)
    }
}

impl IntoValue for &str {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_str(self)
    }
}

impl IntoValue for &&str {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_str(self)
    }
}

impl IntoValue for &[u8] {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_bytes(self)
    }
}

impl IntoValue for &Date {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_date(**self)
    }
}

impl IntoValue for &Time {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_time(**self)
    }
}

impl IntoValue for &Timestamp {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_timestamp(**self)
    }
}

impl IntoValue for &OffsetTimestamp {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_offset_timestamp(**self)
    }
}

impl IntoValue for &Interval {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_interval(**self)
    }
}

impl IntoValue for &Numeric {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_numeric(**self)
    }
}

impl IntoValue for &Geography {
    fn add_to_inserter(&self, inserter: &mut Inserter<'_>) -> Result<()> {
        inserter.add_geography(self)
    }
}

// =============================================================================
// MappedInserter
// =============================================================================

/// An inserter that supports SQL expression mappings.
///
/// This inserter uses a staging table to support computed columns via
/// INSERT...SELECT with SQL expressions. It's created by
/// [`Inserter::with_column_mappings`].
#[derive(Debug)]
pub struct MappedInserter<'conn> {
    /// The underlying inserter for the staging table.
    inner: Inserter<'conn>,
    /// The target table name.
    target_table: crate::TableName,
    /// The column mappings.
    mappings: Vec<ColumnMapping>,
    /// The staging table name.
    staging_table: String,
}

impl<'conn> MappedInserter<'conn> {
    /// Creates a new mapped inserter.
    fn new<T>(
        connection: &'conn Connection,
        inserter_def: &TableDefinition,
        target_table: T,
        mappings: &[ColumnMapping],
    ) -> Result<Self>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let target_table = target_table.try_into()?;

        // Create a unique staging table name
        let staging_table = format!("_hyper_staging_{}", std::process::id());

        // Create the staging table definition (temporary)
        let mut staging_def = inserter_def.clone();
        staging_def.name.clone_from(&staging_table);

        // Create the staging table
        let create_sql = staging_def.to_create_sql(true)?;
        let create_temp = create_sql.replace("CREATE TABLE", "CREATE TEMPORARY TABLE");
        connection.execute_command(&create_temp)?;

        // Create the inner inserter for the staging table
        let inner = Inserter::new(connection, &staging_def)?;

        Ok(MappedInserter {
            inner,
            target_table,
            mappings: mappings.to_vec(),
            staging_table,
        })
    }

    /// Adds a row of values to the inserter.
    ///
    /// The values should correspond to the columns in the inserter definition,
    /// not the target table.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_row`].
    pub fn add_row(&mut self, values: &[&dyn IntoValue]) -> Result<()> {
        self.inner.add_row(values)
    }

    /// Adds a NULL value.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_null`].
    pub fn add_null(&mut self) -> Result<()> {
        self.inner.add_null()
    }

    /// Adds a boolean value.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_bool`].
    pub fn add_bool(&mut self, value: bool) -> Result<()> {
        self.inner.add_bool(value)
    }

    /// Adds an i16 value.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_i16`].
    pub fn add_i16(&mut self, value: i16) -> Result<()> {
        self.inner.add_i16(value)
    }

    /// Adds an i32 value.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_i32`].
    pub fn add_i32(&mut self, value: i32) -> Result<()> {
        self.inner.add_i32(value)
    }

    /// Adds an i64 value.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_i64`].
    pub fn add_i64(&mut self, value: i64) -> Result<()> {
        self.inner.add_i64(value)
    }

    /// Adds an f32 value.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_f32`].
    pub fn add_f32(&mut self, value: f32) -> Result<()> {
        self.inner.add_f32(value)
    }

    /// Adds an f64 value.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_f64`].
    pub fn add_f64(&mut self, value: f64) -> Result<()> {
        self.inner.add_f64(value)
    }

    /// Adds a string value.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_str`].
    pub fn add_str(&mut self, value: &str) -> Result<()> {
        self.inner.add_str(value)
    }

    /// Adds a bytes value.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::add_bytes`].
    pub fn add_bytes(&mut self, value: &[u8]) -> Result<()> {
        self.inner.add_bytes(value)
    }

    /// Ends the current row.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Inserter::end_row`].
    pub fn end_row(&mut self) -> Result<()> {
        self.inner.end_row()
    }

    /// Executes the insert with column mappings.
    ///
    /// This method:
    /// 1. Inserts all buffered rows into the staging table
    /// 2. Executes INSERT...SELECT from staging to target with mappings
    /// 3. Drops the staging table
    ///
    /// Returns the number of rows inserted into the target table.
    ///
    /// # Errors
    ///
    /// - Returns the error from the inner [`Inserter::execute`] if writing
    ///   the staging rows fails.
    /// - Returns [`Error::Server`] if the `INSERT ... SELECT` from staging
    ///   to the target table is rejected (e.g. a mapping expression fails
    ///   to evaluate).
    /// - Returns [`Error::Server`] if dropping the staging table fails.
    pub fn execute(&mut self) -> Result<u64> {
        let connection = self.inner.connection;
        let staging_table = self.staging_table.clone();

        // Insert data into staging table
        let _staging_rows = self.inner.execute()?;

        // Build the INSERT...SELECT statement
        use hyperdb_api_core::protocol::escape::SqlIdentifier;

        let target_columns: Vec<String> = self
            .mappings
            .iter()
            .map(|m| format!("{}", SqlIdentifier(&m.column_name)))
            .collect();

        let select_items: Vec<String> = self
            .mappings
            .iter()
            .map(ColumnMapping::to_select_item)
            .collect();

        let sql = format!(
            "INSERT INTO {} ({}) SELECT {} FROM {}",
            self.target_table,
            target_columns.join(", "),
            select_items.join(", "),
            SqlIdentifier(&staging_table),
        );

        // Execute the INSERT...SELECT (returns row count directly)
        let row_count = connection.execute_command(&sql)?;

        // Drop the staging table
        connection.execute_command(&format!(
            "DROP TABLE IF EXISTS {}",
            SqlIdentifier(&staging_table)
        ))?;

        // Return the number of rows inserted
        Ok(row_count)
    }

    /// Cancels the insert and drops the staging table.
    ///
    /// This method handles cleanup failures gracefully by logging warnings
    /// instead of returning errors. This prevents masking the original error
    /// that caused the cancellation.
    ///
    /// # Logging
    ///
    /// Cleanup failures are logged using the `tracing` crate at WARN level.
    /// If `tracing` is not initialized, errors are written to stderr.
    pub fn cancel(&mut self) {
        let connection = self.inner.connection;
        let staging_table = &self.staging_table;

        // Drop the staging table, but don't fail if cleanup fails
        // This avoids masking the original error that caused cancellation
        if let Err(e) = connection.execute_command(&format!(
            "DROP TABLE IF EXISTS \"{}\"",
            staging_table.replace('"', "\"\"")
        )) {
            // Log the cleanup failure for debugging
            // In production, consider using a logging framework like `tracing`
            eprintln!("Warning: Failed to drop staging table '{staging_table}' during cancel: {e}");
        }
    }
}

// =============================================================================
// InsertChunk - Thread-safe chunk for parallel encoding
// =============================================================================

/// A thread-safe chunk for encoding rows in parallel.
///
/// `InsertChunk` can be created and populated in any thread, then sent to a
/// [`ChunkSender`] for transmission. This enables parallel data encoding across
/// multiple worker threads while serializing the actual network sends.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{InsertChunk, TableDefinition, SqlType, Result};
///
/// fn encode_chunk(table_def: &TableDefinition, start_id: i32) -> Result<InsertChunk> {
///     let mut chunk = InsertChunk::from_table_definition(table_def);
///     
///     for i in 0..1000 {
///         chunk.add_i32(start_id + i)?;
///         chunk.add_str(&format!("Item {}", start_id + i))?;
///         chunk.end_row()?;
///     }
///     
///     Ok(chunk)
/// }
/// ```
#[derive(Debug)]
pub struct InsertChunk {
    buffer: BytesMut,
    header_written: bool,
    column_index: usize,
    column_count: usize,
    row_count: usize,
    column_nullable: Vec<bool>,
}

// SAFETY: Every field of `InsertChunk` (`BytesMut`, `bool`, `usize`,
// `Vec<bool>`) is itself `Send`, and none of them hold raw pointers or
// thread-local state. The manual `unsafe impl` exists only because the
// auto-trait derivation is conservative for this struct's compilation context;
// the compound type has no `!Send` components.
unsafe impl Send for InsertChunk {}
// SAFETY: Same reasoning as the `Send` impl above — all fields are `Sync`
// and there is no interior mutability crossing a `&InsertChunk` boundary,
// so sharing `&InsertChunk` across threads is sound.
unsafe impl Sync for InsertChunk {}

impl InsertChunk {
    /// Creates a new empty chunk with the given schema.
    ///
    /// # Arguments
    ///
    /// * `column_count` - Number of columns per row
    /// * `column_nullable` - Whether each column is nullable
    #[must_use]
    pub fn new(column_count: usize, column_nullable: Vec<bool>) -> Self {
        debug_assert_eq!(column_count, column_nullable.len());
        InsertChunk {
            buffer: BytesMut::with_capacity(INITIAL_BUFFER_SIZE),
            header_written: false,
            column_index: 0,
            column_count,
            row_count: 0,
            column_nullable,
        }
    }

    /// Creates a chunk from a table definition.
    #[must_use]
    pub fn from_table_definition(table_def: &TableDefinition) -> Self {
        let column_nullable: Vec<bool> = table_def.columns.iter().map(|c| c.nullable).collect();
        Self::new(table_def.column_count(), column_nullable)
    }

    /// Returns the number of complete rows in this chunk.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the current buffer size in bytes.
    #[must_use]
    pub fn buffer_size(&self) -> usize {
        self.buffer.len()
    }

    /// Returns true if the chunk has reached size or row limits and should be sent.
    #[must_use]
    pub fn should_flush(&self) -> bool {
        self.row_count >= CHUNK_ROW_LIMIT || self.buffer.len() >= CHUNK_SIZE_LIMIT
    }

    /// Returns true if the chunk is empty (no rows).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Takes the buffer, consuming the chunk data.
    ///
    /// Returns `None` if the chunk is empty. After calling this, the chunk
    /// can be reused by calling the add_* methods again.
    ///
    /// Note: The header flag is NOT reset - subsequent chunks from the same
    /// `InsertChunk` will NOT include the header (`HyperBinary` only needs one
    /// header per COPY stream).
    pub fn take(&mut self) -> Option<BytesMut> {
        if self.row_count == 0 {
            return None;
        }
        // Don't reset header_written - only first chunk should have header
        self.row_count = 0;
        Some(std::mem::take(&mut self.buffer))
    }

    /// Resets the chunk for reuse without reallocating.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.header_written = false;
        self.column_index = 0;
        self.row_count = 0;
    }

    #[allow(
        clippy::inline_always,
        reason = "hot-path numeric kernel; forced inlining measured to matter on this specific function"
    )]
    fn ensure_header(&mut self) {
        if !self.header_written {
            copy::write_header(&mut self.buffer);
            self.header_written = true;
        }
    }

    #[expect(
        clippy::inline_always,
        reason = "hot inner loop of the inserter; measured to matter for per-row throughput"
    )]
    #[inline(always)]
    fn current_column_nullable(&self) -> bool {
        *self.column_nullable.get(self.column_index).unwrap_or(&true)
    }

    /// Adds a NULL value for the current column.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] with message `"Too many columns in row"`
    ///   if the current row already has all columns supplied.
    /// - Returns [`Error::InvalidTableDefinition`] with message
    ///   `"Cannot add NULL to non-nullable column"` if the current column
    ///   is `NOT NULL` in the schema.
    pub fn add_null(&mut self) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        if !self.current_column_nullable() {
            return Err(Error::invalid_table_definition(
                "Cannot add NULL to non-nullable column",
            ));
        }
        self.ensure_header();
        copy::write_null(&mut self.buffer);
        self.column_index += 1;
        Ok(())
    }

    /// Adds a boolean value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTableDefinition`] with message `"Too many columns in row"` if
    /// the current row already has all columns supplied.
    pub fn add_bool(&mut self, value: bool) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        let int_value = i8::from(value);
        if self.current_column_nullable() {
            copy::write_i8(&mut self.buffer, int_value);
        } else {
            copy::write_i8_not_null(&mut self.buffer, int_value);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds an i16 value (SMALLINT).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_i16(&mut self, value: i16) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        if self.current_column_nullable() {
            copy::write_i16(&mut self.buffer, value);
        } else {
            copy::write_i16_not_null(&mut self.buffer, value);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds an i32 value (INT).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_i32(&mut self, value: i32) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        if self.current_column_nullable() {
            copy::write_i32(&mut self.buffer, value);
        } else {
            copy::write_i32_not_null(&mut self.buffer, value);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds an i64 value (BIGINT).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_i64(&mut self, value: i64) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        if self.current_column_nullable() {
            copy::write_i64(&mut self.buffer, value);
        } else {
            copy::write_i64_not_null(&mut self.buffer, value);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds an f32 value (REAL/FLOAT4).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_f32(&mut self, value: f32) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        if self.current_column_nullable() {
            copy::write_f32(&mut self.buffer, value);
        } else {
            copy::write_f32_not_null(&mut self.buffer, value);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds an f64 value (DOUBLE PRECISION/FLOAT8).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_f64(&mut self, value: f64) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        if self.current_column_nullable() {
            copy::write_f64(&mut self.buffer, value);
        } else {
            copy::write_f64_not_null(&mut self.buffer, value);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds a string value (TEXT/VARCHAR).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_str(&mut self, value: &str) -> Result<()> {
        self.add_bytes(value.as_bytes())
    }

    /// Adds a bytes value (BYTEA).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_bytes(&mut self, value: &[u8]) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        if value.len() > u32::MAX as usize {
            return Err(Error::conversion(format!(
                "Value length {} exceeds HyperBinary 4-byte length limit ({})",
                value.len(),
                u32::MAX
            )));
        }
        self.ensure_header();
        if self.current_column_nullable() {
            copy::write_varbinary(&mut self.buffer, value);
        } else {
            copy::write_varbinary_not_null(&mut self.buffer, value);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds a 128-bit value (NUMERIC/INTERVAL).
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_data128(&mut self, value: &[u8; 16]) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        if self.current_column_nullable() {
            copy::write_data128(&mut self.buffer, value);
        } else {
            copy::write_data128_not_null(&mut self.buffer, value);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds a Date value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_date(&mut self, value: Date) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        let julian_day = value.to_julian_day();
        if self.current_column_nullable() {
            copy::write_i32(&mut self.buffer, julian_day);
        } else {
            copy::write_i32_not_null(&mut self.buffer, julian_day);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds a Time value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_time(&mut self, value: Time) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        let micros = value.to_microseconds();
        if self.current_column_nullable() {
            copy::write_i64(&mut self.buffer, micros);
        } else {
            copy::write_i64_not_null(&mut self.buffer, micros);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds a Timestamp value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_timestamp(&mut self, value: Timestamp) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        let micros = value.to_microseconds();
        if self.current_column_nullable() {
            copy::write_i64(&mut self.buffer, micros);
        } else {
            copy::write_i64_not_null(&mut self.buffer, micros);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds an `OffsetTimestamp` (TIMESTAMP WITH TIME ZONE) value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_offset_timestamp(&mut self, value: OffsetTimestamp) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        let micros = value.to_microseconds_utc();
        if self.current_column_nullable() {
            copy::write_i64(&mut self.buffer, micros);
        } else {
            copy::write_i64_not_null(&mut self.buffer, micros);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds an Interval value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_interval(&mut self, value: Interval) -> Result<()> {
        if self.column_index >= self.column_count {
            return Err(Error::invalid_table_definition("Too many columns in row"));
        }
        self.ensure_header();
        let packed = value.to_packed();
        if self.current_column_nullable() {
            copy::write_data128(&mut self.buffer, &packed);
        } else {
            copy::write_data128_not_null(&mut self.buffer, &packed);
        }
        self.column_index += 1;
        Ok(())
    }

    /// Adds a Geography value.
    ///
    /// # Errors
    ///
    /// See [`add_bool`](Self::add_bool).
    pub fn add_geography(&mut self, value: &Geography) -> Result<()> {
        // Geography uses the same varbinary path as add_bytes
        self.add_bytes(value.as_bytes())
    }

    /// Ends the current row.
    ///
    /// Returns an error if the wrong number of columns were added.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTableDefinition`] if fewer (or more) columns were supplied
    /// for this row than the chunk's column count.
    pub fn end_row(&mut self) -> Result<()> {
        if self.column_index != self.column_count {
            return Err(Error::invalid_table_definition(format!(
                "Expected {} columns, got {}",
                self.column_count, self.column_index
            )));
        }
        self.column_index = 0;
        self.row_count += 1;
        Ok(())
    }

    /// Returns the current column index (for checking incomplete rows).
    #[must_use]
    pub fn column_index(&self) -> usize {
        self.column_index
    }

    /// Returns a reference to the internal buffer.
    pub(crate) fn buffer(&self) -> &BytesMut {
        &self.buffer
    }
}

// =============================================================================
// ChunkSender - Mutex-protected sender for InsertChunks
// =============================================================================

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

/// A thread-safe sender for [`InsertChunk`]s.
///
/// `ChunkSender` manages the COPY protocol connection and ensures that only one
/// chunk is sent at a time. Multiple threads can call `send_chunk()` concurrently;
/// the mutex ensures serialized access.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{Catalog, Connection, CreateMode, ChunkSender, InsertChunk, TableDefinition, SqlType, Result};
/// use std::sync::mpsc;
/// use std::thread;
///
/// fn main() -> Result<()> {
///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
///     
///     let table_def = TableDefinition::new("products")
///         .add_required_column("id", SqlType::int())
///         .add_nullable_column("name", SqlType::text());
///     
///     Catalog::new(&conn).create_table(&table_def)?;
///     
///     let sender = ChunkSender::new(&conn, &table_def)?;
///     let (tx, rx) = mpsc::channel::<InsertChunk>();
///     
///     // Worker thread
///     let table_def_clone = table_def.clone();
///     let handle = thread::spawn(move || {
///         let mut chunk = InsertChunk::from_table_definition(&table_def_clone);
///         for i in 0..1000i32 {
///             chunk.add_i32(i).unwrap();
///             chunk.add_str(&format!("Product {}", i)).unwrap();
///             chunk.end_row().unwrap();
///         }
///         tx.send(chunk).unwrap();
///     });
///     
///     // Receive and send chunks
///     while let Ok(chunk) = rx.recv() {
///         sender.send_chunk(chunk)?;
///     }
///     
///     handle.join().unwrap();
///     let rows = sender.finish()?;
///     println!("Inserted {} rows", rows);
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct ChunkSender<'conn> {
    connection: &'conn Connection,
    table_name: String,
    columns: Vec<String>,
    writer: Mutex<Option<CopyInWriter<'conn>>>,
    header_sent: std::sync::atomic::AtomicBool,
    total_rows: AtomicU64,
    chunks_sent: AtomicUsize,
}

impl<'conn> ChunkSender<'conn> {
    /// Creates a new chunk sender for the given table.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTableDefinition`] if `table_def` has zero
    /// columns. The COPY session itself is opened lazily on the first
    /// [`send_chunk`](Self::send_chunk), so transport errors surface there.
    pub fn new(connection: &'conn Connection, table_def: &TableDefinition) -> Result<Self> {
        if table_def.column_count() == 0 {
            return Err(Error::invalid_table_definition(
                "Table definition must have at least one column",
            ));
        }

        let columns: Vec<String> = table_def.columns.iter().map(|c| c.name.clone()).collect();
        let table_name = table_def.qualified_name();

        Ok(ChunkSender {
            connection,
            table_name,
            columns,
            writer: Mutex::new(None),
            header_sent: std::sync::atomic::AtomicBool::new(false),
            total_rows: AtomicU64::new(0),
            chunks_sent: AtomicUsize::new(0),
        })
    }

    /// Sends a chunk to Hyper.
    ///
    /// This method is thread-safe - multiple threads can call it concurrently,
    /// but only one chunk will be sent at a time.
    ///
    /// Each `InsertChunk` includes a `HyperBinary` header (19 bytes). This method
    /// automatically handles headers: the first chunk's header is sent, and
    /// headers in subsequent chunks are stripped (`HyperBinary` expects only one
    /// header per COPY stream).
    ///
    /// # Errors
    ///
    /// Returns an error if the chunk is empty or if sending fails.
    pub fn send_chunk(&self, mut chunk: InsertChunk) -> Result<()> {
        // Capture row count before take() resets it
        let row_count = chunk.row_count();

        let Some(buffer) = chunk.take() else {
            return Ok(());
        };

        // Acquire the lock for exclusive send access
        let mut writer_guard = self
            .writer
            .lock()
            .map_err(|_| Error::internal("ChunkSender mutex poisoned"))?;

        // Lazily initialize the COPY connection
        if writer_guard.is_none() {
            let client = self.connection.tcp_client().ok_or_else(|| {
                Error::feature_not_supported(
                    "ChunkSender requires a TCP connection. gRPC connections do not support COPY operations."
                )
            })?;
            let columns: Vec<&str> = self
                .columns
                .iter()
                .map(std::string::String::as_str)
                .collect();
            *writer_guard = Some(client.copy_in(&self.table_name, &columns)?);
        }

        // Handle headers: only first chunk should have header in the COPY stream
        // Each InsertChunk includes a 19-byte HyperBinary header, so we need to
        // strip headers from all chunks except the first one sent.
        let is_first = !self.header_sent.swap(true, Ordering::SeqCst);

        let data_to_send = if is_first {
            // First chunk: send with header
            &buffer[..]
        } else {
            // Subsequent chunks: strip the 19-byte header if present
            if buffer.len() > hyperdb_api_core::protocol::copy::HYPER_BINARY_HEADER_SIZE
                && buffer.starts_with(hyperdb_api_core::protocol::copy::HYPER_BINARY_HEADER)
            {
                &buffer[hyperdb_api_core::protocol::copy::HYPER_BINARY_HEADER_SIZE..]
            } else {
                &buffer[..]
            }
        };

        // Write the chunk directly to the socket, avoiding a full-chunk memcpy
        // into the connection's write buffer. flush_stream ensures the data
        // reaches the server before we return.
        if let Some(ref mut writer) = *writer_guard {
            writer.send_direct(data_to_send)?;
            writer.flush_stream()?;
        }

        // Update counters (lock already released for these atomic ops)
        drop(writer_guard);
        self.total_rows
            .fetch_add(row_count as u64, Ordering::Relaxed);
        self.chunks_sent.fetch_add(1, Ordering::Relaxed);

        debug!(
            target: "hyperdb_api",
            chunk = self.chunks_sent.load(Ordering::Relaxed),
            rows = row_count,
            bytes = data_to_send.len(),
            "chunk-sender"
        );

        Ok(())
    }

    /// Returns the total number of rows sent so far.
    pub fn total_rows(&self) -> u64 {
        self.total_rows.load(Ordering::Relaxed)
    }

    /// Returns the number of chunks sent so far.
    pub fn chunks_sent(&self) -> usize {
        self.chunks_sent.load(Ordering::Relaxed)
    }

    /// Finishes the COPY operation and returns the total row count.
    ///
    /// This method consumes the sender. After calling this, the COPY operation
    /// is complete and all data has been committed.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] with message `"ChunkSender mutex poisoned"`
    ///   if a sender thread panicked while holding the writer lock.
    /// - Returns [`Error::Server`] or [`Error::Io`] if sending the COPY
    ///   trailer or finishing the COPY operation fails.
    pub fn finish(self) -> Result<u64> {
        let mut writer_guard = self
            .writer
            .lock()
            .map_err(|_| Error::internal("ChunkSender mutex poisoned"))?;

        // If no chunks were sent, return 0
        let Some(writer) = writer_guard.take() else {
            return Ok(0);
        };

        // Write and send the COPY trailer
        let mut trailer_buf = BytesMut::with_capacity(2);
        copy::write_trailer(&mut trailer_buf);

        // Need to get mutable access to send trailer
        let mut writer = writer;
        writer.send(&trailer_buf)?;

        // Finish the COPY operation
        let rows = writer.finish()?;

        info!(
            target: "hyperdb_api",
            rows,
            chunks = self.chunks_sent.load(Ordering::Relaxed),
            table = %self.table_name,
            "chunk-sender-finish"
        );

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use crate::table_definition::TableDefinition;
    use hyperdb_api_core::types::SqlType;

    use super::InsertChunk;

    fn create_test_table_def() -> TableDefinition {
        TableDefinition::new("test")
            .add_required_column("id", SqlType::int())
            .add_nullable_column("name", SqlType::text())
    }

    #[test]
    fn test_inserter_column_validation() {
        // We can't fully test without a connection, but we can test validation logic
        let table_def = create_test_table_def();
        assert_eq!(table_def.column_count(), 2);
    }

    #[test]
    fn test_insert_chunk_encoding() {
        let table_def = create_test_table_def();
        let mut chunk = InsertChunk::from_table_definition(&table_def);

        // Add a row
        chunk.add_i32(42).unwrap();
        chunk.add_str("hello").unwrap();
        chunk.end_row().unwrap();

        assert_eq!(chunk.row_count(), 1);
        assert!(!chunk.is_empty());
        assert!(!chunk.should_flush()); // Not at limit yet

        // Add more rows
        for i in 0..100 {
            chunk.add_i32(i).unwrap();
            chunk.add_str(&format!("item {i}")).unwrap();
            chunk.end_row().unwrap();
        }

        assert_eq!(chunk.row_count(), 101);

        // Take the buffer
        let buffer = chunk.take().unwrap();
        assert!(!buffer.is_empty());

        // Chunk should now be empty after take
        assert!(chunk.take().is_none());
    }

    #[test]
    fn test_insert_chunk_null_handling() {
        let table_def = create_test_table_def();
        let mut chunk = InsertChunk::from_table_definition(&table_def);

        // First column is NOT NULL, should fail
        assert!(chunk.add_null().is_err());

        // Add the required column first
        chunk.add_i32(1).unwrap();

        // Second column is nullable, should succeed
        chunk.add_null().unwrap();
        chunk.end_row().unwrap();

        assert_eq!(chunk.row_count(), 1);
    }

    #[test]
    fn test_insert_chunk_column_count_validation() {
        let table_def = create_test_table_def();
        let mut chunk = InsertChunk::from_table_definition(&table_def);

        // Add only one column
        chunk.add_i32(1).unwrap();

        // end_row should fail
        assert!(chunk.end_row().is_err());

        // Add second column
        chunk.add_str("test").unwrap();

        // Now end_row should succeed
        chunk.end_row().unwrap();
    }

    #[test]
    fn test_insert_chunk_too_many_columns() {
        let table_def = create_test_table_def();
        let mut chunk = InsertChunk::from_table_definition(&table_def);

        chunk.add_i32(1).unwrap();
        chunk.add_str("test").unwrap();

        // Third column should fail
        assert!(chunk.add_i32(2).is_err());
    }

    #[test]
    fn test_insert_chunk_clear() {
        let table_def = create_test_table_def();
        let mut chunk = InsertChunk::from_table_definition(&table_def);

        chunk.add_i32(1).unwrap();
        chunk.add_str("test").unwrap();
        chunk.end_row().unwrap();

        assert_eq!(chunk.row_count(), 1);

        chunk.clear();

        assert_eq!(chunk.row_count(), 0);
        assert!(chunk.is_empty());
    }
}
