// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! gRPC-based connection to Hyper databases.
//!
//! This module provides [`GrpcConnection`] for query-only access to Hyper databases
//! via gRPC, with results returned in Apache Arrow IPC format.
//!
//! # Differences from TCP Connection
//!
//! gRPC connections have these limitations compared to TCP:
//!
//! - **Read-only**: Only SELECT queries are supported
//! - **No DDL/DML**: CREATE, DROP, INSERT, UPDATE, DELETE not supported
//! - **No COPY**: Bulk insertion via COPY protocol not available
//! - **No prepared statements**: Parameters not supported
//!
//! Use [`Connection`](crate::Connection) for full read-write access.
//!
//! # Transfer Modes
//!
//! gRPC queries support three transfer modes via
//! [`TransferMode`](crate::grpc::TransferMode), which control how
//! result data flows between server and client:
//!
//! - **`Sync`** — All results are returned in a single response message.
//!   Simple but limited to a 100-second server timeout, making it unsuitable
//!   for long-running queries or large result sets.
//!
//! - **`Async`** — The initial response contains only a header (query ID);
//!   results must be fetched separately via `GetQueryResult`. Useful when
//!   the client needs to decouple query submission from result consumption.
//!
//! - **`Adaptive`** (default, recommended) — The first chunk of results is
//!   returned inline with the response; remaining chunks are streamed via
//!   `GetQueryResult`. This avoids the sync timeout limit while keeping
//!   latency low for small results that fit in a single chunk.
//!
//! # Example
//!
//! ```no_run
//! use hyperdb_api::grpc::{GrpcConnection, GrpcConnectionAsync};
//!
//! // Async usage
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut conn = GrpcConnectionAsync::connect(
//!         "http://localhost:7484",
//!         "my_database.hyper"
//!     ).await?;
//!
//!     // Execute a query
//!     let arrow_data = conn.execute_query_to_arrow("SELECT * FROM users").await?;
//!
//!     // Or get a structured result
//!     let result = conn.execute_query("SELECT id, name FROM users").await?;
//!     println!("Got {} bytes of Arrow data", result.arrow_data().len());
//!
//!     Ok(())
//! }
//!
//! // Sync usage
//! fn main_sync() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut conn = GrpcConnection::connect(
//!         "http://localhost:7484",
//!         "my_database.hyper"
//!     )?;
//!
//!     let arrow_data = conn.execute_query_to_arrow("SELECT * FROM users")?;
//!     Ok(())
//! }
//! ```

use crate::error::Result;

// Re-export types from hyperdb_api_core::client::grpc for convenience
pub(crate) use hyperdb_api_core::client::grpc::{
    GrpcClient, GrpcClientSync, GrpcConfig, GrpcQueryResult,
};

/// A gRPC connection to a Hyper database (query-only).
///
/// Unlike [`Connection`](crate::Connection), this provides read-only access via gRPC
/// with results in Apache Arrow IPC format. This is useful for:
///
/// - Load-balanced deployments where gRPC provides better routing
/// - Integration with Arrow-based data pipelines
/// - Scenarios where HTTP/2 benefits are needed
///
/// # Limitations
///
/// gRPC connections only support SELECT queries. Attempting to execute
/// DDL (CREATE, DROP), DML (INSERT, UPDATE, DELETE), or use features like
/// prepared statements will result in an error.
///
/// # Async vs Sync
///
/// This struct provides both async and sync APIs:
///
/// - `connect()` / `execute_query()` - blocking (uses internal tokio runtime)
/// - `connect_async()` / `execute_query_async()` - async (requires tokio runtime)
///
/// For applications already using tokio, the async methods are preferred.
#[derive(Debug)]
pub struct GrpcConnection {
    /// The underlying gRPC client (sync wrapper)
    client: GrpcClientSync,
    /// Database path
    database: Option<String>,
}

impl GrpcConnection {
    /// Connects to a Hyper server via gRPC (blocking).
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The gRPC endpoint URL (e.g., "<http://localhost:7484>")
    /// * `database_path` - Path to the database to query
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::grpc::GrpcConnection;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let conn = GrpcConnection::connect(
    ///     "http://localhost:7484",
    ///     "my_database.hyper"
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Connection`] wrapping a
    /// `hyperdb_api_core::client::Error` if the HTTP/2 channel cannot be established
    /// (endpoint unreachable, TLS handshake failure, auth rejection).
    pub fn connect(endpoint: &str, database_path: &str) -> Result<Self> {
        let config = GrpcConfig::new(endpoint).database(database_path);
        let client = GrpcClientSync::connect(config)?;

        Ok(GrpcConnection {
            client,
            database: Some(database_path.to_string()),
        })
    }

    /// Connects to a Hyper server via gRPC with custom configuration (blocking).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::grpc::{GrpcConnection, GrpcConfig};
    /// # use std::time::Duration;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let config = GrpcConfig::new("http://localhost:7484")
    ///     .database("my_database.hyper")
    ///     .connect_timeout(Duration::from_secs(10))
    ///     .header("x-custom-header", "value");
    ///
    /// let conn = GrpcConnection::connect_with_config(config)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Connection`] if the HTTP/2 channel cannot be
    /// established with the supplied configuration.
    pub fn connect_with_config(config: GrpcConfig) -> Result<Self> {
        let database = config.database_path().map(std::string::ToString::to_string);
        let client = GrpcClientSync::connect(config)?;

        Ok(GrpcConnection { client, database })
    }

    /// Executes a SQL query and returns raw Arrow IPC bytes (blocking).
    ///
    /// This is the most efficient method when you need the raw Arrow data
    /// for processing with an Arrow library.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::grpc::GrpcConnection;
    /// # fn example(conn: &mut GrpcConnection) -> Result<(), Box<dyn std::error::Error>> {
    /// let arrow_bytes = conn.execute_query_to_arrow("SELECT * FROM users")?;
    ///
    /// // Process with arrow crate using the zero-copy helper
    /// let rowset = hyperdb_api::ArrowRowset::from_bytes(arrow_bytes)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Server`] if the gRPC server rejects the query
    /// or if the HTTP/2 channel fails mid-stream.
    pub fn execute_query_to_arrow(&mut self, sql: &str) -> Result<bytes::Bytes> {
        Ok(self.client.execute_query_to_arrow(sql)?)
    }

    /// Executes a SQL query and returns a structured result (blocking).
    ///
    /// The result contains the Arrow IPC data along with metadata about
    /// the query execution.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::grpc::GrpcConnection;
    /// # fn example(conn: &mut GrpcConnection) -> Result<(), Box<dyn std::error::Error>> {
    /// let result = conn.execute_query("SELECT id, name FROM users")?;
    /// println!("Query ID: {:?}", result.query_id());
    /// println!("Columns: {}", result.column_count());
    /// let arrow_data = result.into_arrow_data();
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Server`] if the gRPC server rejects the query
    /// or the HTTP/2 channel fails.
    pub fn execute_query(&mut self, sql: &str) -> Result<GrpcQueryResult> {
        Ok(self.client.execute_query(sql)?)
    }

    /// Cancels an in-flight gRPC query by its `query_id` (blocking).
    ///
    /// This is the gRPC analogue of PG wire's `CancelRequest`. Unlike PG
    /// wire — where the cancel opens a separate TCP connection — the
    /// gRPC cancel rides the existing HTTP/2 channel as a regular RPC,
    /// so it reuses this connection's channel, database routing, and
    /// custom headers.
    ///
    /// # When you have a `query_id`
    ///
    /// The server assigns a `query_id` for ASYNC-mode queries
    /// (long-running queries that the client polls).  Read it from
    /// [`GrpcQueryResult::query_id`] after an async-mode execution.
    /// SYNC-mode queries typically complete before a cancel would be
    /// useful — drop the in-flight future instead.
    ///
    /// Cancellation is best-effort: a successful `Ok(())` return means
    /// the server acknowledged the cancel, not that the query had not
    /// already finished. Errors indicate a transport-level failure
    /// (channel closed, network error, auth expired) — useful for
    /// metrics, retry logic, or "cancel failed" UX.
    ///
    /// # Fallible by design
    ///
    /// The `Result<()>` return is the **explicit user-facing cancel
    /// API** and is distinct from the
    /// [`Cancellable`](hyperdb_api_core::client::Cancellable) trait, which requires
    /// an infallible `cancel(&self)` method with no arguments. A
    /// `GrpcConnection` cannot implement `Cancellable` directly: the
    /// trait's signature has nowhere to pass a `query_id`, and gRPC
    /// connections can carry many concurrent queries (so there is no
    /// unambiguous "the" query to cancel the way there is on a PG wire
    /// connection). If you need `Cancellable`-style fire-and-forget
    /// cancel for a future gRPC streaming result type, it will live
    /// on a per-query handle that wraps this method and swallows
    /// errors — mirroring
    /// [`impl Cancellable for hyperdb_api_core::client::Client`](hyperdb_api_core::client::Cancellable).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::grpc::GrpcConnection;
    /// # fn example(conn: &mut GrpcConnection, query_id: &str) -> hyperdb_api::Result<()> {
    /// conn.cancel_query(query_id)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Connection`] on transport-level failures (channel
    /// closed, network error, auth expired). An unknown or already-finished
    /// `query_id` is not an error — the server returns `Ok`.
    pub fn cancel_query(&mut self, query_id: &str) -> Result<()> {
        Ok(self.client.cancel_query(query_id)?)
    }

    /// Returns the database path, if one is attached.
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// Returns the gRPC configuration.
    pub fn config(&self) -> &GrpcConfig {
        self.client.config()
    }

    /// Closes the connection (blocking).
    ///
    /// Note: The connection is automatically closed when dropped.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Connection`] if the underlying HTTP/2 channel cannot
    /// be shut down cleanly.
    pub fn close(self) -> Result<()> {
        Ok(self.client.close()?)
    }
}

/// Async gRPC connection to a Hyper database (query-only).
///
/// This is the async version of [`GrpcConnection`], designed for use with
/// tokio-based async applications.
///
/// # Example
///
/// ```no_run
/// # use hyperdb_api::grpc::GrpcConnectionAsync;
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let mut conn = GrpcConnectionAsync::connect(
///         "http://localhost:7484",
///         "my_database.hyper"
///     ).await?;
///
///     let arrow_data = conn.execute_query_to_arrow("SELECT * FROM users").await?;
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct GrpcConnectionAsync {
    /// The underlying async gRPC client
    client: GrpcClient,
    /// Database path
    database: Option<String>,
}

impl GrpcConnectionAsync {
    /// Connects to a Hyper server via gRPC (async).
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The gRPC endpoint URL (e.g., "<http://localhost:7484>")
    /// * `database_path` - Path to the database to query
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Connection`] if the HTTP/2 channel cannot be
    /// established (endpoint unreachable, TLS handshake failure).
    pub async fn connect(endpoint: &str, database_path: &str) -> Result<Self> {
        let config = GrpcConfig::new(endpoint).database(database_path);
        let client = GrpcClient::connect(config).await?;

        Ok(GrpcConnectionAsync {
            client,
            database: Some(database_path.to_string()),
        })
    }

    /// Connects to a Hyper server via gRPC with custom configuration (async).
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Connection`] if the HTTP/2 channel cannot be
    /// established with the supplied configuration.
    pub async fn connect_with_config(config: GrpcConfig) -> Result<Self> {
        let database = config.database_path().map(std::string::ToString::to_string);
        let client = GrpcClient::connect(config).await?;

        Ok(GrpcConnectionAsync { client, database })
    }

    /// Executes a SQL query and returns raw Arrow IPC bytes (async).
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Server`] if the server rejects the query or the
    /// HTTP/2 channel fails mid-stream.
    pub async fn execute_query_to_arrow(&mut self, sql: &str) -> Result<bytes::Bytes> {
        Ok(self.client.execute_query_to_arrow(sql).await?)
    }

    /// Executes a SQL query and returns a structured result (async).
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Server`] if the server rejects the query or the
    /// HTTP/2 channel fails.
    pub async fn execute_query(&mut self, sql: &str) -> Result<GrpcQueryResult> {
        Ok(self.client.execute_query(sql).await?)
    }

    /// Cancels an in-flight gRPC query by its `query_id` (async).
    ///
    /// See [`GrpcConnection::cancel_query`] for full semantics, including
    /// the "Fallible by design" discussion of why this returns
    /// `Result<()>` and why it is *not* an implementation of the
    /// [`Cancellable`](hyperdb_api_core::client::Cancellable) trait. The async
    /// variant avoids blocking the current thread; both variants route
    /// the cancel over the same channel used for queries, carrying this
    /// connection's database routing and custom headers.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::grpc::GrpcConnectionAsync;
    /// # async fn example(conn: &mut GrpcConnectionAsync, query_id: &str)
    /// #     -> hyperdb_api::Result<()> {
    /// conn.cancel_query(query_id).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// See [`GrpcConnection::cancel_query`].
    pub async fn cancel_query(&mut self, query_id: &str) -> Result<()> {
        Ok(self.client.cancel_query(query_id).await?)
    }

    /// Returns the database path, if one is attached.
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// Returns the gRPC configuration.
    pub fn config(&self) -> &GrpcConfig {
        self.client.config()
    }

    /// Closes the connection (async).
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Connection`] if the underlying HTTP/2 channel cannot
    /// be shut down cleanly.
    pub async fn close(self) -> Result<()> {
        Ok(self.client.close().await?)
    }
}

// ============================================================================
// Streaming chunk source adapter
// ============================================================================

/// Adapter from a [`hyperdb_api_core::client::grpc::GrpcChunkStreamSync`] to
/// [`crate::arrow_result::ChunkSource`].
///
/// Used internally by [`Connection::execute_query`](crate::Connection::execute_query)
/// on a gRPC transport so the high-level `Rowset` decodes record batches
/// lazily instead of buffering the whole Arrow IPC payload in memory.
pub(crate) struct GrpcChunkStreamSource {
    inner: hyperdb_api_core::client::grpc::GrpcChunkStreamSync,
}

impl GrpcChunkStreamSource {
    pub(crate) fn new(inner: hyperdb_api_core::client::grpc::GrpcChunkStreamSync) -> Self {
        GrpcChunkStreamSource { inner }
    }
}

impl crate::arrow_result::ChunkSource for GrpcChunkStreamSource {
    fn next_chunk(&mut self) -> crate::Result<Option<bytes::Bytes>> {
        Ok(self.inner.next_chunk()?)
    }
}
