// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Arrow IPC stream reader for query results.
//!
//! This module provides the [`ArrowReader`] struct for reading query results
//! in Arrow IPC stream format.
//!
//! Unlike regular query execution which returns row-by-row results, `ArrowReader`
//! returns complete Arrow IPC streams, which can be directly consumed by Arrow
//! libraries like the `arrow` crate.
//!
//! # Example
//!
//! ```no_run
//! # use hyperdb_api::{ArrowReader, Connection, Result};
//! # fn example(conn: &Connection) -> Result<()> {
//! use hyperdb_api::{ArrowReader, Connection};
//!
//! let reader = ArrowReader::new(&conn);
//!
//! // Query results as Arrow IPC stream
//! let arrow_data = reader.query_to_arrow("SELECT * FROM my_table")?;
//!
//! // Or export an entire table
//! let arrow_data = reader.table_to_arrow("my_table")?;
//! # Ok(())
//! # }
//! ```

use crate::connection::Connection;
use crate::error::Result;

/// Reads query results in Arrow IPC stream format.
///
/// `ArrowReader` provides methods to execute queries and receive results as
/// Arrow IPC stream data. This is useful for integration with Arrow-based
/// data processing pipelines.
///
/// # How It Works
///
/// Internally, `ArrowReader` uses `COPY (SELECT ...) TO STDOUT WITH (format arrowstream)`
/// to retrieve query results in Arrow format. The returned bytes are a valid
/// Arrow IPC stream containing:
/// 1. A schema message
/// 2. One or more record batch messages
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{ArrowReader, Connection, CreateMode, Result};
///
/// fn main() -> Result<()> {
///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
///
///     // Create and populate a table
///     conn.execute_command("CREATE TABLE data (id INT, value DOUBLE PRECISION)")?;
///     conn.execute_command("INSERT INTO data VALUES (1, 1.5), (2, 2.5), (3, 3.5)")?;
///
///     // Read the table as Arrow
///     let reader = ArrowReader::new(&conn);
///     let arrow_data = reader.table_to_arrow("data")?;
///
///     println!("Got {} bytes of Arrow IPC data", arrow_data.len());
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct ArrowReader<'conn> {
    connection: &'conn Connection,
}

impl<'conn> ArrowReader<'conn> {
    /// Creates a new Arrow reader for the given connection.
    pub fn new(connection: &'conn Connection) -> Self {
        ArrowReader { connection }
    }

    /// Executes a SELECT query and returns results as Arrow IPC stream.
    ///
    /// The query should be a SELECT statement. It will be wrapped in a
    /// `COPY (...) TO STDOUT WITH (format arrowstream)` to retrieve the
    /// results in Arrow format.
    ///
    /// # Arguments
    ///
    /// * `select_query` - A SELECT query (without COPY wrapper)
    ///
    /// # Returns
    ///
    /// Raw Arrow IPC stream bytes that can be parsed by Arrow libraries.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowReader, Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let reader = ArrowReader::new(&conn);
    /// let arrow_data = reader.query_to_arrow("SELECT id, name FROM users WHERE active = true")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`crate::Error::FeatureNotSupported`] if the connection is using gRPC
    ///   transport (ArrowReader wraps `COPY TO STDOUT`, which is TCP-only).
    /// - Returns [`crate::Error::Server`] if the server rejects the
    ///   `COPY (<query>) TO STDOUT WITH (format arrowstream)` statement.
    /// - Returns [`crate::Error::Io`] on transport-level I/O failures.
    pub fn query_to_arrow(&self, select_query: &str) -> Result<Vec<u8>> {
        let copy_query = format!("COPY ({select_query}) TO STDOUT WITH (format arrowstream)");
        let client = self.connection.tcp_client().ok_or_else(|| {
            crate::Error::feature_not_supported(
                "ArrowReader requires a TCP connection. Use Connection::execute_query_to_arrow() for gRPC."
            )
        })?;
        Ok(client.copy_out(&copy_query)?)
    }

    /// Exports an entire table to Arrow IPC stream format.
    ///
    /// This is equivalent to `query_to_arrow("SELECT * FROM table_name")`.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The table name (should be properly escaped if needed)
    ///
    /// # Returns
    ///
    /// Raw Arrow IPC stream bytes containing all rows from the table.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowReader, Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let reader = ArrowReader::new(&conn);
    /// let arrow_data = reader.table_to_arrow("my_table")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// See [`query_to_arrow`](Self::query_to_arrow).
    pub fn table_to_arrow(&self, table_name: &str) -> Result<Vec<u8>> {
        self.query_to_arrow(&format!("SELECT * FROM {table_name}"))
    }

    /// Exports specific columns from a table to Arrow IPC stream format.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The table name
    /// * `columns` - Column names to export
    ///
    /// # Returns
    ///
    /// Raw Arrow IPC stream bytes containing the specified columns.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowReader, Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let reader = ArrowReader::new(&conn);
    /// let arrow_data = reader.table_columns_to_arrow("users", &["id", "name", "email"])?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// See [`query_to_arrow`](Self::query_to_arrow).
    pub fn table_columns_to_arrow(&self, table_name: &str, columns: &[&str]) -> Result<Vec<u8>> {
        let column_list = columns.join(", ");
        self.query_to_arrow(&format!("SELECT {column_list} FROM {table_name}"))
    }

    /// Exports a table with a WHERE clause to Arrow IPC stream format.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The table name
    /// * `where_clause` - The WHERE clause (without the "WHERE" keyword)
    ///
    /// # Returns
    ///
    /// Raw Arrow IPC stream bytes containing filtered rows.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{ArrowReader, Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let reader = ArrowReader::new(&conn);
    /// let arrow_data = reader.table_filtered_to_arrow("users", "active = true")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// See [`query_to_arrow`](Self::query_to_arrow).
    pub fn table_filtered_to_arrow(&self, table_name: &str, where_clause: &str) -> Result<Vec<u8>> {
        self.query_to_arrow(&format!("SELECT * FROM {table_name} WHERE {where_clause}"))
    }
}

#[cfg(test)]
mod tests {
    // Integration tests require a running Hyper instance
    // See hyperdb-api/tests/arrow_reader_tests.rs
}
