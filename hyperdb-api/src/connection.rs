// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Database connection management.
//!
//! The [`Connection`] type provides a unified interface for connecting to Hyper
//! databases via either TCP (`PostgreSQL` wire protocol) or gRPC transport.
//! The transport is automatically detected from the endpoint URL:
//!
//! - `https://` or `http://` → gRPC transport
//! - Otherwise → TCP transport (e.g., `localhost:7483`)

use std::path::Path;

use hyperdb_api_core::client::Client;

use crate::error::{Error, Result};
use crate::names::escape_sql_path;
use crate::process::HyperProcess;
use crate::result::{Row, Rowset, DEFAULT_BINARY_CHUNK_SIZE};
use crate::transport::Transport;

use std::any::Any;
use std::sync::{Arc, Mutex};

use crate::query_stats::{QueryStats, QueryStatsProvider};

/// Trait for types that can be extracted from a scalar query result.
///
/// This trait enables the generic [`Connection::execute_scalar_query`] method,
/// similar to C++'s `executeScalarQuery<T>()` template.
///
/// # Implementing Custom Types
///
/// You can implement this trait for custom types to use them with `execute_scalar_query`:
///
/// ```no_run
/// # use hyperdb_api::{Row, ScalarValue};
/// # struct MyType;
/// # impl MyType { fn parse(s: &str) -> Self { MyType } }
/// impl ScalarValue for MyType {
///     fn from_row(row: &Row, col: usize) -> Option<Self> {
///         row.get_string(col).map(|s| MyType::parse(&s))
///     }
/// }
/// ```
pub trait ScalarValue: Sized {
    /// Extracts a value of this type from a row at the given column.
    fn from_row(row: &Row, col: usize) -> Option<Self>;
}

impl ScalarValue for i64 {
    fn from_row(row: &Row, col: usize) -> Option<Self> {
        row.get_i64(col)
    }
}

impl ScalarValue for i32 {
    fn from_row(row: &Row, col: usize) -> Option<Self> {
        row.get_i32(col)
    }
}

impl ScalarValue for i16 {
    fn from_row(row: &Row, col: usize) -> Option<Self> {
        row.get_i16(col)
    }
}

impl ScalarValue for f64 {
    fn from_row(row: &Row, col: usize) -> Option<Self> {
        row.get_f64(col)
    }
}

impl ScalarValue for bool {
    fn from_row(row: &Row, col: usize) -> Option<Self> {
        row.get_bool(col)
    }
}

impl ScalarValue for String {
    fn from_row(row: &Row, col: usize) -> Option<Self> {
        row.get_string(col)
    }
}

/// Database creation mode when connecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CreateMode {
    /// Do not create the database. Method will fail if database doesn't exist.
    #[default]
    DoNotCreate,
    /// Create the database. Method will fail if the database already exists.
    Create,
    /// Create the database if it doesn't exist.
    CreateIfNotExists,
    /// Create the database. If it already exists, drop the old one first.
    CreateAndReplace,
}

/// A connection to a Hyper database.
///
/// This struct represents an active connection to a Hyper server and optionally
/// an attached database. The connection is automatically closed when dropped.
///
/// # Transport Auto-Detection
///
/// The transport is automatically detected from the endpoint URL:
/// - `https://` or `http://` → gRPC transport (read-only until server supports writes)
/// - Otherwise → TCP transport (full read/write support)
///
/// # CSV / Text Import & Export
///
/// For CSV, TSV, and other delimited-text formats, see the [`copy`](crate::copy)
/// module which provides [`export_csv()`](Self::export_csv),
/// [`import_csv()`](Self::import_csv), and related methods on this struct.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{Connection, CreateMode, Result};
///
/// fn main() -> Result<()> {
///     // TCP connection (full read/write)
///     let conn = Connection::connect("localhost:7483", "example.hyper", CreateMode::CreateIfNotExists)?;
///
///     // Execute SQL commands
///     conn.execute_command("CREATE TABLE test (id INT, name TEXT)")?;
///     conn.execute_command("INSERT INTO test VALUES (1, 'Hello')")?;
///
///     Ok(())
/// }
/// ```
///
/// ```no_run
/// # use hyperdb_api::{Connection, CreateMode, Result};
/// # fn example() -> Result<()> {
/// // gRPC connection (read-only, auto-detected from URL)
/// let conn = Connection::connect(
///     "https://hyper-server.example.com:443",
///     "example.hyper",
///     CreateMode::DoNotCreate,  // Must be DoNotCreate for gRPC
/// )?;
/// # Ok(())
/// # }
/// ```
pub struct Connection {
    transport: Transport,
    database: Option<String>,
    stats_provider: Option<Arc<dyn QueryStatsProvider>>,
    /// Pending stats token + SQL from the most recent query, resolved lazily.
    pending_stats: Mutex<Option<(Box<dyn Any + Send>, String)>>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Creates a new connection to a Hyper instance with a database.
    ///
    /// This is the primary way to connect to a running [`HyperProcess`].
    ///
    /// # Arguments
    ///
    /// * `instance` - The Hyper server instance to connect to.
    /// * `database_path` - Path to the database file.
    /// * `create_mode` - How to handle database creation.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection could not be established.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{HyperProcess, Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let hyper = HyperProcess::new(None, None)?;
    ///     let conn = Connection::new(&hyper, "database.hyper", CreateMode::CreateIfNotExists)?;
    ///     Ok(())
    /// }
    /// ```
    pub fn new(
        instance: &HyperProcess,
        database_path: impl AsRef<Path>,
        create_mode: CreateMode,
    ) -> Result<Self> {
        // Prefer using the connection_endpoint which properly handles UDS/Named Pipes
        if let Some(conn_endpoint) = instance.connection_endpoint() {
            return Self::connect_with_endpoint(
                conn_endpoint,
                &database_path.as_ref().to_string_lossy(),
                create_mode,
            );
        }

        // Fall back to string endpoint (TCP)
        let endpoint = instance.require_endpoint()?;
        Self::connect(
            endpoint,
            &database_path.as_ref().to_string_lossy(),
            create_mode,
        )
    }

    /// Connects using a `ConnectionEndpoint` (supports TCP, UDS, and Named Pipes).
    fn connect_with_endpoint(
        endpoint: &hyperdb_api_core::client::ConnectionEndpoint,
        database_path: &str,
        create_mode: CreateMode,
    ) -> Result<Self> {
        let db_path_str = Some(database_path.to_string());

        let config = hyperdb_api_core::client::Config::new().with_user("tableau_internal_user");

        let client = hyperdb_api_core::client::Client::connect_endpoint(endpoint, &config)?;

        let conn = Connection::from_client(client, db_path_str.clone());

        // Handle database creation
        if let Some(db_path) = db_path_str {
            conn.handle_creation_mode(&db_path, create_mode)?;
            conn.attach_and_set_path(&db_path)?;
        }

        Ok(conn)
    }

    /// Connects to a Hyper server and optionally attaches a database.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The server endpoint (host:port).
    /// * `database_path` - Path to the database file.
    /// * `create_mode` - How to handle database creation.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection could not be established.
    pub fn connect(endpoint: &str, database_path: &str, create_mode: CreateMode) -> Result<Self> {
        crate::ConnectionBuilder::new(endpoint)
            .database(database_path)
            .create_mode(create_mode)
            .build()
    }

    /// Returns a connection builder for advanced configuration.
    ///
    /// This is useful when you need to set authentication, timeouts, or
    /// other advanced options before connecting.
    #[must_use]
    pub fn builder(endpoint: &str) -> crate::ConnectionBuilder {
        crate::ConnectionBuilder::new(endpoint)
    }

    /// Creates a Connection from a low-level Client (internal use, TCP only).
    pub(crate) fn from_client(client: Client, database: Option<String>) -> Self {
        Connection {
            transport: Transport::Tcp(Box::new(crate::transport::TcpTransport { client })),
            database,
            stats_provider: None,
            pending_stats: Mutex::new(None),
        }
    }

    /// Creates a Connection from a Transport (internal use).
    #[allow(
        dead_code,
        reason = "used by ConnectionBuilder for the gRPC path; not reached under non-gRPC feature builds"
    )]
    pub(crate) fn from_transport(transport: Transport, database: Option<String>) -> Self {
        Connection {
            transport,
            database,
            stats_provider: None,
            pending_stats: Mutex::new(None),
        }
    }

    /// Returns the transport type name (e.g., "TCP", "gRPC", "Unix Socket").
    pub fn transport_type(&self) -> &'static str {
        self.transport.transport_type().as_str()
    }

    /// Returns true if this connection supports write operations.
    ///
    /// Currently, only TCP connections support writes. gRPC connections are
    /// read-only until the server supports write operations over gRPC.
    pub fn supports_writes(&self) -> bool {
        self.transport.supports_writes()
    }

    /// Handles database creation logic (internal use).
    pub(crate) fn handle_creation_mode(
        &self,
        database_path: &str,
        create_mode: CreateMode,
    ) -> Result<()> {
        match create_mode {
            CreateMode::DoNotCreate => {}
            CreateMode::Create => {
                self.execute_command(&format!(
                    "CREATE DATABASE {}",
                    escape_sql_path(database_path)
                ))?;
            }
            CreateMode::CreateIfNotExists => {
                if let Err(e) = self.execute_command(&format!(
                    "CREATE DATABASE IF NOT EXISTS {}",
                    escape_sql_path(database_path)
                )) {
                    if !is_already_exists_error(&e) {
                        return Err(Error::internal(format!(
                            "Failed to create database '{database_path}': {e}"
                        )));
                    }
                }
            }
            CreateMode::CreateAndReplace => {
                let _ = self.execute_command(&format!(
                    "DROP DATABASE IF EXISTS {}",
                    escape_sql_path(database_path)
                ));
                self.execute_command(&format!(
                    "CREATE DATABASE {}",
                    escape_sql_path(database_path)
                ))?;
            }
        }
        Ok(())
    }

    /// Attaches and sets the database path (internal use).
    pub(crate) fn attach_and_set_path(&self, database_path: &str) -> Result<()> {
        let db_alias = std::path::Path::new(database_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("db");

        self.execute_command(&format!(
            "ATTACH DATABASE {} AS {}",
            escape_sql_path(database_path),
            escape_sql_path(db_alias)
        ))?;

        self.execute_command(&format!(
            "SET search_path TO {}, public",
            escape_sql_path(db_alias)
        ))?;

        Ok(())
    }

    /// Connects to a Hyper server with authentication.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The server endpoint (host:port).
    /// * `database_path` - Path to the database file.
    /// * `create_mode` - How to handle database creation.
    /// * `user` - Username for authentication.
    /// * `password` - Password for authentication.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or authentication fails.
    pub fn connect_with_auth(
        endpoint: &str,
        database_path: &str,
        create_mode: CreateMode,
        user: &str,
        password: &str,
    ) -> Result<Self> {
        crate::ConnectionBuilder::new(endpoint)
            .database(database_path)
            .create_mode(create_mode)
            .user(user.to_string())
            .password(password)
            .build()
    }

    /// Creates a connection to a Hyper server without attaching a database.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Connection`] if the TCP or gRPC handshake fails, and
    /// [`Error::Io`] if the endpoint cannot be reached.
    pub fn without_database(endpoint: &str) -> Result<Self> {
        crate::ConnectionBuilder::new(endpoint).build()
    }

    /// Executes a SQL command that doesn't return results.
    ///
    /// Use this for DDL statements (CREATE, ALTER, DROP) and DML statements
    /// (INSERT, UPDATE, DELETE).
    ///
    /// # Arguments
    ///
    /// * `command` - The SQL command to execute.
    ///
    /// # Returns
    ///
    /// The number of affected rows, or 0 if not applicable.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The connection is using gRPC transport (write operations not yet supported)
    /// - The command fails to execute
    pub fn execute_command(&self, command: &str) -> Result<u64> {
        let token = self.stats_before_query(command);

        let result = self.transport.execute_command(command);

        // For commands, the query is fully executed synchronously, so we
        // can store the pending token immediately for lazy resolution.
        self.stats_store_pending(token, command);

        result
    }

    /// Executes a SQL query and returns a streaming result set.
    ///
    /// Results are streamed in chunks (default 64K rows), keeping memory usage
    /// constant regardless of result set size. This makes it safe for any
    /// result size, from a single row to billions of rows.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let mut result = conn.execute_query("SELECT id, value FROM measurements")?;
    /// while let Some(chunk) = result.next_chunk()? {
    ///     for row in &chunk {
    ///         // Generic typed access (like C++ row.get<T>())
    ///         let id: Option<i32> = row.get(0);
    ///         let value: Option<f64> = row.get(1);
    ///
    ///         // Or direct accessors
    ///         let id = row.get_i32(0);
    ///         let value = row.get_f64(1);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Memory Behavior
    ///
    /// - Only one chunk is held in memory at a time (~few MB for 64K rows)
    /// - Safe for result sets of any size (millions/billions of rows)
    /// - Memory usage is `O(chunk_size)`, not `O(total_rows)`
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Server`] wrapping a `hyperdb_api_core::client::Error` if the
    ///   SQL fails to parse, execute, or if the server reports an error
    ///   while streaming.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub fn execute_query(&self, query: &str) -> Result<Rowset<'_>> {
        let token = self.stats_before_query(query);

        let result = match &self.transport {
            Transport::Tcp(tcp) => {
                let stream = tcp
                    .client
                    .query_streaming(query, DEFAULT_BINARY_CHUNK_SIZE)?;
                Ok(Rowset::new(stream))
            }
            Transport::Grpc(grpc) => {
                // gRPC streaming: pull chunks lazily so peak memory is
                // bounded by one gRPC message (tonic default 64 MB), not
                // by the full result size. Matches TCP's
                // constant-memory streaming shape.
                //
                // The transport module already creates a fresh gRPC client
                // per query (gRPC client needs &mut self to execute), so we
                // do the same here: connect, start the stream, wrap as a
                // `ChunkSource`. The stream keeps the channel and runtime
                // alive via refcounted handles inside `GrpcChunkStreamSync`.
                let mut client =
                    hyperdb_api_core::client::grpc::GrpcClientSync::connect(grpc.config.clone())?;
                let stream = client.execute_query_stream(query)?;
                let source = Box::new(crate::grpc_connection::GrpcChunkStreamSource::new(stream));
                let arrow_rowset = crate::arrow_result::ArrowRowset::from_stream(source)?;
                Ok(Rowset::from_arrow(arrow_rowset))
            }
        };

        // Store the pending token — Hyper logs the execution stats after the
        // result is consumed (streamed), so we defer resolution until
        // last_query_stats() is called.
        self.stats_store_pending(token, query);

        result
    }

    // =========================================================================
    // Arrow Format Queries
    // =========================================================================

    /// Executes a SELECT query and returns results as Arrow IPC stream bytes.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
    ///
    ///     // Create and populate a table
    ///     conn.execute_command("CREATE TABLE data (id INT, value DOUBLE PRECISION)")?;
    ///     conn.execute_command("INSERT INTO data VALUES (1, 1.5), (2, 2.5)")?;
    ///
    ///     // Get results as Arrow IPC stream
    ///     let arrow_data = conn.execute_query_to_arrow("SELECT * FROM data")?;
    ///     println!("Got {} bytes of Arrow IPC data", arrow_data.len());
    ///
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Propagates any [`Error::Server`] from the TCP or gRPC transport when
    /// the query fails or the server cannot produce Arrow IPC output.
    pub fn execute_query_to_arrow(&self, select_query: &str) -> Result<bytes::Bytes> {
        self.transport.execute_query_to_arrow(select_query)
    }

    /// Exports an entire table to Arrow IPC stream format.
    ///
    /// This is a convenience method equivalent to
    /// `execute_query_to_arrow("SELECT * FROM table_name")`.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The table name
    ///
    /// # Returns
    ///
    /// Raw Arrow IPC stream bytes containing all rows from the table.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
    ///     let arrow_data = conn.export_table_to_arrow("my_table")?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns whatever [`execute_query_to_arrow`](Self::execute_query_to_arrow)
    /// would return for `SELECT * FROM <table_name>` — typically
    /// [`Error::Server`] if the table does not exist or the query is rejected.
    pub fn export_table_to_arrow(&self, table_name: &str) -> Result<bytes::Bytes> {
        self.execute_query_to_arrow(&format!("SELECT * FROM {table_name}"))
    }

    /// Executes a SELECT query and returns results as Arrow `RecordBatch`es.
    ///
    /// This is the recommended method for Arrow-native workflows (`DataFusion`,
    /// Polars, etc.) where you want direct `RecordBatch` access without going
    /// through the `Row` abstraction.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    /// use arrow::record_batch::RecordBatch;
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///
    ///     let batches: Vec<RecordBatch> = conn.execute_query_to_batches("SELECT * FROM data")?;
    ///     for batch in &batches {
    ///         println!("batch: {} rows x {} cols", batch.num_rows(), batch.num_columns());
    ///     }
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Server`] if the query itself fails.
    /// - Returns [`Error::Conversion`] if the Arrow IPC payload returned by the
    ///   server is malformed and cannot be decoded into record batches.
    pub fn execute_query_to_batches(
        &self,
        select_query: &str,
    ) -> Result<Vec<arrow::record_batch::RecordBatch>> {
        let arrow_data = self.execute_query_to_arrow(select_query)?;
        crate::arrow_result::parse_arrow_ipc(arrow_data)
    }

    /// Fetches a single row from a query.
    ///
    /// Returns an error if the query returns no rows.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     let row = conn.fetch_one("SELECT * FROM users WHERE id = 1")?;
    ///     let id: Option<i32> = row.get(0);
    ///     let name: Option<String> = row.get(1);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns the error from [`execute_query`](Self::execute_query) if
    ///   the query itself fails.
    /// - Returns [`Error::Conversion`] with message `"Query returned no rows"` if
    ///   the query produced zero rows.
    pub fn fetch_one<Q>(&self, query: Q) -> Result<crate::Row>
    where
        Q: AsRef<str>,
    {
        let query = query.as_ref();
        let result = self.execute_query(query)?;
        result.require_first_row()
    }

    /// Fetches an optional single row from a query.
    ///
    /// Returns `None` if the query returns no rows.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     if let Some(row) = conn.fetch_optional("SELECT * FROM users WHERE id = 999")? {
    ///         let name: Option<String> = row.get(1);
    ///         println!("Found user: {:?}", name);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the error from [`execute_query`](Self::execute_query) if the
    /// query itself fails. An empty result set is not an error — it yields
    /// `Ok(None)`.
    pub fn fetch_optional<Q>(&self, query: Q) -> Result<Option<crate::Row>>
    where
        Q: AsRef<str>,
    {
        let query = query.as_ref();
        let result = self.execute_query(query)?;
        result.first_row()
    }

    /// Fetches all rows from a query.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     let rows = conn.fetch_all("SELECT * FROM users WHERE active = true ORDER BY name")?;
    ///     for row in rows {
    ///         let id: Option<i32> = row.get(0);
    ///         let name: Option<String> = row.get(1);
    ///         println!("User {}: {:?}", id.unwrap_or(-1), name);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the error from [`execute_query`](Self::execute_query), or a
    /// transport error produced while draining every chunk of the streamed
    /// result set.
    pub fn fetch_all<Q>(&self, query: Q) -> Result<Vec<crate::Row>>
    where
        Q: AsRef<str>,
    {
        let query = query.as_ref();
        let result = self.execute_query(query)?;
        result.collect_rows()
    }

    /// Fetches a single row and maps it to a struct using [`FromRow`](crate::FromRow).
    ///
    /// Returns an error if the query returns no rows or if mapping fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, FromRow, RowAccessor, Result};
    ///
    /// struct User { id: i32, name: String }
    ///
    /// impl FromRow for User {
    ///     fn from_row(row: RowAccessor<'_>) -> Result<Self> {
    ///         Ok(User {
    ///             id: row.get("id")?,
    ///             name: row.get_opt("name")?.unwrap_or_default(),
    ///         })
    ///     }
    /// }
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     let user: User = conn.fetch_one_as("SELECT id, name FROM users WHERE id = 1")?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns the error from [`fetch_one`](Self::fetch_one) if the query
    ///   fails or returns no rows.
    /// - Returns whatever error [`FromRow::from_row`](crate::FromRow::from_row)
    ///   produces when the row cannot be mapped into `T`.
    pub fn fetch_one_as<T: crate::FromRow>(&self, query: &str) -> Result<T> {
        let row = self.fetch_one(query)?;
        let indices = row
            .schema()
            .map(crate::row_accessor::RowAccessor::build_indices)
            .unwrap_or_default();
        T::from_row(crate::RowAccessor::new(&row, &indices))
    }

    /// Fetches all rows and maps them to structs using [`FromRow`](crate::FromRow).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, FromRow, RowAccessor, Result};
    /// # struct User { id: i32, name: String }
    /// # impl FromRow for User {
    /// #     fn from_row(row: RowAccessor<'_>) -> Result<Self> {
    /// #         Ok(User { id: row.get("id")?, name: row.get_opt("name")?.unwrap_or_default() })
    /// #     }
    /// # }
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let users: Vec<User> = conn.fetch_all_as("SELECT id, name FROM users")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns the error from [`fetch_all`](Self::fetch_all) if the query
    ///   fails.
    /// - Returns the first error produced by
    ///   [`FromRow::from_row`](crate::FromRow::from_row) on any of the rows.
    pub fn fetch_all_as<T: crate::FromRow>(&self, query: &str) -> Result<Vec<T>> {
        let rows = self.fetch_all(query)?;
        // Build the column-name → index lookup once from the first
        // row's schema; reuse for every row. All rows in a result set
        // share the same `Arc<ResultSchema>`, so this is safe.
        let indices = rows
            .first()
            .and_then(crate::result::Row::schema)
            .map(crate::row_accessor::RowAccessor::build_indices)
            .unwrap_or_default();
        rows.iter()
            .map(|r| T::from_row(crate::RowAccessor::new(r, &indices)))
            .collect()
    }

    /// Fetches a single scalar value from a query.
    ///
    /// Returns an error if the query returns no rows or NULL.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM users")?;
    ///     println!("User count: {}", count);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns the error from [`execute_query`](Self::execute_query) if
    ///   the query itself fails.
    /// - Returns [`Error::Conversion`] with message `"Query returned no rows"` if
    ///   the query produced zero rows.
    /// - Returns [`Error::Conversion`] with message `"Scalar query returned NULL"`
    ///   if the single cell is SQL `NULL`.
    pub fn fetch_scalar<T, Q>(&self, query: Q) -> Result<T>
    where
        T: crate::connection::ScalarValue + crate::result::RowValue,
        Q: AsRef<str>,
    {
        let query = query.as_ref();
        let result = self.execute_query(query)?;
        result.require_scalar()
    }

    /// Fetches an optional scalar value from a query.
    ///
    /// Returns `None` if the query returns no rows or NULL.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     let max_id: Option<i32> = conn.fetch_optional_scalar("SELECT MAX(id) FROM users")?;
    ///     println!("Max ID: {:?}", max_id);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns the error from [`execute_query`](Self::execute_query) if
    ///   the query itself fails.
    /// - Returns [`Error::Conversion`] with message `"Query returned no rows"` if
    ///   the query produced zero rows. (An empty result is treated as an
    ///   error here because we need at least one row to inspect; SQL `NULL`
    ///   in the single cell yields `Ok(None)`.)
    pub fn fetch_optional_scalar<T, Q>(&self, query: Q) -> Result<Option<T>>
    where
        T: crate::connection::ScalarValue + crate::result::RowValue,
        Q: AsRef<str>,
    {
        let query = query.as_ref();
        let result = self.execute_query(query)?;
        result.scalar()
    }

    /// Executes a scalar query and returns a single value of type `T`.
    ///
    /// Alias for [`fetch_optional_scalar`](Self::fetch_optional_scalar) for C++ API compatibility.
    ///
    /// # Errors
    ///
    /// See [`fetch_optional_scalar`](Self::fetch_optional_scalar).
    #[inline]
    pub fn execute_scalar_query<T>(&self, query: &str) -> Result<Option<T>>
    where
        T: ScalarValue + crate::result::RowValue,
    {
        self.fetch_optional_scalar(query)
    }

    /// Queries for a count value, defaulting to 0 if NULL.
    ///
    /// This is optimized for COUNT queries which typically return 0
    /// instead of NULL when there are no matching rows.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     let count = conn.query_count("SELECT COUNT(*) FROM users WHERE active = true")?;
    ///     println!("Active users: {}", count);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the error from [`execute_query`](Self::execute_query) if the
    /// query fails or produces no rows. SQL `NULL` is mapped to `0`, not an
    /// error.
    pub fn query_count(&self, query: &str) -> Result<i64> {
        self.fetch_optional_scalar::<i64, _>(query)
            .map(|opt| opt.unwrap_or(0))
    }

    // =========================================================================
    // Parameterized Queries (SQL Injection Safe)
    // =========================================================================

    /// Executes a parameterized query, returning streaming results.
    ///
    /// This is safe to use with untrusted user input: parameters travel
    /// through the extended query protocol (Parse/Bind/Execute) as
    /// binary `HyperBinary` values and are never interpolated into the
    /// SQL string. For repeated executions of the same SQL with different
    /// values, prefer the explicit [`prepare`](Self::prepare) API — it
    /// returns a reusable [`PreparedStatement`](crate::PreparedStatement)
    /// that skips the Parse round-trip on every call.
    ///
    /// Under the hood, `query_params` is a one-shot
    /// prepare+execute+close: it prepares an unnamed statement, binds
    /// the parameters, starts streaming, and closes the statement when
    /// the returned [`Rowset`] is dropped.
    ///
    /// # Arguments
    ///
    /// * `query` - The SQL query with parameter placeholders (`$1`, `$2`, etc.)
    /// * `params` - Parameter values matching the placeholders
    ///
    /// # SQL Injection Prevention
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn search_users(conn: &Connection, user_input: &str) -> Result<()> {
    ///     // DANGEROUS - vulnerable to SQL injection:
    ///     // let query = format!("SELECT * FROM users WHERE name = '{}'", user_input);
    ///
    ///     // SAFE - parameterized query:
    ///     let mut result = conn.query_params(
    ///         "SELECT * FROM users WHERE name = $1",
    ///         &[&user_input],
    ///     )?;
    ///
    ///     while let Some(chunk) = result.next_chunk()? {
    ///         for row in &chunk {
    ///             let id: Option<i32> = row.get(0);
    ///             let name: Option<String> = row.get(1);
    ///             println!("Found: {:?} - {:?}", id, name);
    ///         }
    ///     }
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Multiple Parameters
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///
    ///     // Multiple parameters of different types
    ///     let result = conn.query_params(
    ///         "SELECT * FROM orders WHERE customer_id = $1 AND total > $2",
    ///         &[&42i32, &100.0f64],
    ///     )?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] if the connection is using gRPC transport
    ///   (prepared statements are TCP-only).
    /// - Returns [`Error::Server`] if the server rejects the statement at
    ///   `Parse`, `Bind`, or `Execute` time, including on type-mismatch
    ///   between `params` and the inferred OIDs.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub fn query_params(
        &self,
        query: &str,
        params: &[&dyn crate::params::ToSqlParam],
    ) -> Result<Rowset<'_>> {
        // Implementation note: routes through the extended query protocol
        // via Parse/Bind/Execute so parameters travel in HyperBinary
        // format — no SQL escaping, full SQL-injection safety regardless of
        // parameter content. The statement handle is stashed inside the
        // returned Rowset so its Drop-time close_statement fires *after*
        // the rowset releases its connection lock (otherwise the close
        // would deadlock on the still-held mutex).
        let client = match &self.transport {
            Transport::Tcp(tcp) => &tcp.client,
            Transport::Grpc(_) => {
                return Err(Error::feature_not_supported(
                    "prepared statements are not supported over gRPC transport",
                ));
            }
        };
        let oids: Vec<crate::Oid> = params.iter().map(|p| p.sql_oid()).collect();
        let stmt = client.prepare_typed(query, &oids)?;
        let encoded: Vec<Option<Vec<u8>>> = params.iter().map(|p| p.encode_param()).collect();
        let stream =
            client.execute_streaming(&stmt, encoded, crate::result::DEFAULT_BINARY_CHUNK_SIZE)?;
        Ok(Rowset::from_prepared(stream).with_statement_guard(stmt))
    }

    /// Executes a parameterized command that doesn't return rows.
    ///
    /// Use this for INSERT, UPDATE, DELETE, or DDL statements with parameters.
    /// Returns the number of affected rows.
    ///
    /// See [`query_params`](Self::query_params) for details on parameter
    /// handling and SQL injection prevention.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn delete_user(conn: &Connection, user_id: i32) -> Result<u64> {
    ///     // Safe from SQL injection
    ///     conn.command_params("DELETE FROM users WHERE id = $1", &[&user_id])
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] if the connection is using gRPC transport.
    /// - Returns [`Error::Server`] if the server rejects the statement at
    ///   `Parse`, `Bind`, or `Execute` time.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub fn command_params(
        &self,
        query: &str,
        params: &[&dyn crate::params::ToSqlParam],
    ) -> Result<u64> {
        // One-shot prepare+execute with explicit OIDs — see `query_params`
        // for why we collect OIDs from each parameter.
        let client = match &self.transport {
            Transport::Tcp(tcp) => &tcp.client,
            Transport::Grpc(_) => {
                return Err(Error::feature_not_supported(
                    "prepared statements are not supported over gRPC transport",
                ));
            }
        };
        let oids: Vec<crate::Oid> = params.iter().map(|p| p.sql_oid()).collect();
        let stmt = client.prepare_typed(query, &oids)?;
        let encoded: Vec<Option<Vec<u8>>> = params.iter().map(|p| p.encode_param()).collect();
        Ok(client.execute_no_result(&stmt, encoded)?)
    }

    /// Executes multiple SQL statements in a single call.
    ///
    /// Each statement is executed sequentially. If any statement fails,
    /// execution stops and the error is returned. Returns the total number
    /// of affected rows across all statements.
    ///
    /// This is more efficient than calling `execute_command` in a loop
    /// because it reduces round-trips for DDL scripts and multi-statement setup.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     let total = conn.execute_batch(&[
    ///         "CREATE TABLE users (id INT, name TEXT)",
    ///         "INSERT INTO users VALUES (1, 'Alice')",
    ///         "INSERT INTO users VALUES (2, 'Bob')",
    ///     ])?;
    ///     println!("Total affected: {}", total);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a wrapped [`Error::Internal`] on the first statement that fails;
    /// its `source` is the original [`Error::Server`] from
    /// [`execute_command`](Self::execute_command). The error message
    /// includes the failing statement's ordinal and an 80-character preview
    /// of its SQL.
    pub fn execute_batch(&self, statements: &[&str]) -> Result<u64> {
        let mut total = 0u64;
        for (i, stmt) in statements.iter().enumerate() {
            if !stmt.trim().is_empty() {
                total += self.execute_command(stmt).map_err(|e| {
                    let preview: String = stmt.chars().take(80).collect();
                    Error::internal(format!(
                        "execute_batch failed at statement {} of {}: {}: {}",
                        i + 1,
                        statements.len(),
                        preview,
                        e,
                    ))
                })?;
            }
        }
        Ok(total)
    }

    /// Returns the attached database path, if any.
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// Creates a new database file.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::without_database("localhost:7483")?;
    ///     conn.create_database("new_database.hyper")?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects the
    /// `CREATE DATABASE IF NOT EXISTS` statement (e.g. the path is not
    /// writable on the server).
    pub fn create_database(&self, path: &str) -> Result<()> {
        let sql = format!("CREATE DATABASE IF NOT EXISTS {}", escape_sql_path(path));
        self.execute_command(&sql)?;
        Ok(())
    }

    /// Drops (deletes) a database file.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::without_database("localhost:7483")?;
    ///     conn.drop_database("old_database.hyper")?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects the
    /// `DROP DATABASE IF EXISTS` statement (e.g. the database is still
    /// attached or permissions deny deletion).
    pub fn drop_database(&self, path: &str) -> Result<()> {
        let sql = format!("DROP DATABASE IF EXISTS {}", escape_sql_path(path));
        self.execute_command(&sql)?;
        Ok(())
    }

    /// Attaches a database file to the connection.
    ///
    /// Once attached, the database can be queried and modified.
    /// The database is identified by its alias (or by its path if no alias is provided).
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the database file to attach.
    /// * `alias` - Optional alias for the database. If `None`, the database is
    ///   attached without an explicit alias (typically using its filename).
    ///
    /// # Errors
    ///
    /// Returns an error if the database file doesn't exist or if attachment fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::without_database("localhost:7483")?;
    ///
    ///     // Attach with an alias
    ///     conn.attach_database("data.hyper", Some("mydata"))?;
    ///
    ///     // Attach without an alias
    ///     conn.attach_database("other.hyper", None)?;
    ///     Ok(())
    /// }
    /// ```
    pub fn attach_database(&self, path: &str, alias: Option<&str>) -> Result<()> {
        let sql = if let Some(alias) = alias {
            format!(
                "ATTACH DATABASE {} AS {}",
                escape_sql_path(path),
                escape_sql_path(alias)
            )
        } else {
            format!("ATTACH DATABASE {}", escape_sql_path(path))
        };
        self.execute_command(&sql)?;
        Ok(())
    }

    /// Detaches a database from this connection.
    ///
    /// After detaching, the database file is released and can be accessed
    /// externally (e.g., copied, moved, etc.). All pending updates are
    /// written to disk before detaching.
    ///
    /// # Arguments
    ///
    /// * `alias` - The alias of the database to detach.
    ///
    /// # Errors
    ///
    /// Returns an error if the database is not attached or if detachment fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::without_database("localhost:7483")?;
    ///     conn.attach_database("data.hyper", Some("mydata"))?;
    ///     // ... work with the database ...
    ///     conn.detach_database("mydata")?;
    ///     Ok(())
    /// }
    /// ```
    pub fn detach_database(&self, alias: &str) -> Result<()> {
        let sql = format!("DETACH DATABASE {}", escape_sql_path(alias));
        self.execute_command(&sql)?;
        Ok(())
    }

    /// Detaches all databases from this connection.
    ///
    /// This is useful for cleanup before closing a connection or when
    /// you need to release all database files.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects the
    /// `DETACH ALL DATABASES` statement (e.g. a database is still in use by
    /// another session).
    pub fn detach_all_databases(&self) -> Result<()> {
        self.execute_command("DETACH ALL DATABASES")?;
        Ok(())
    }

    /// Creates a schema in the database.
    ///
    /// # Errors
    ///
    /// - Returns an error if `schema_name` cannot be converted into a
    ///   [`SchemaName`](crate::SchemaName) (invalid identifier).
    /// - Returns [`Error::Server`] if the server rejects the
    ///   `CREATE SCHEMA` statement (e.g. the schema already exists).
    pub fn create_schema<T>(&self, schema_name: T) -> Result<()>
    where
        T: TryInto<crate::SchemaName>,
        crate::Error: From<T::Error>,
    {
        crate::catalog::Catalog::new(self).create_schema(schema_name)
    }

    /// Checks whether a schema exists.
    ///
    /// # Arguments
    ///
    /// * `schema` - The schema name (can include database qualifier).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::without_database("localhost:7483")?;
    ///     if conn.has_schema("public")? {
    ///         println!("Schema 'public' exists");
    ///     }
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns an error if `schema` cannot be converted into a
    ///   [`SchemaName`](crate::SchemaName).
    /// - Returns [`Error::Server`] if the catalog lookup query fails.
    pub fn has_schema<T>(&self, schema: T) -> Result<bool>
    where
        T: TryInto<crate::SchemaName>,
        crate::Error: From<T::Error>,
    {
        use crate::catalog::Catalog;
        Catalog::new(self).has_schema(schema)
    }

    /// Checks whether a table exists.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The table name (can include database and schema qualifiers).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::without_database("localhost:7483")?;
    ///     if conn.has_table("public.users")? {
    ///         println!("Table 'users' exists");
    ///     }
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns an error if `table_name` cannot be converted into a
    ///   [`TableName`](crate::TableName).
    /// - Returns [`Error::Server`] if the catalog lookup query fails.
    pub fn has_table<T>(&self, table_name: T) -> Result<bool>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        use crate::catalog::Catalog;
        Catalog::new(self).has_table(table_name)
    }

    /// Returns the server version as a parsed struct.
    ///
    /// Returns `None` if the version cannot be determined (e.g., gRPC connection).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, HyperProcess, ServerVersion, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let hyper = HyperProcess::new(None, None)?;
    ///     let conn = Connection::new(&hyper, "test.hyper", CreateMode::CreateIfNotExists)?;
    ///     if let Some(version) = conn.server_version() {
    ///         println!("Hyper {}", version);
    ///         if version >= ServerVersion::new(0, 1, 0) {
    ///             println!("Has feature X");
    ///         }
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub fn server_version(&self) -> Option<crate::ServerVersion> {
        let version_str = self.parameter_status("server_version")?;
        crate::ServerVersion::parse(&version_str)
    }

    /// Copies a database file to a new path.
    ///
    /// The source database must be attached to this connection.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let hyper = HyperProcess::new(None, None)?;
    ///     let conn = Connection::new(&hyper, "source.hyper", CreateMode::DoNotCreate)?;
    ///     conn.copy_database("source.hyper", "backup.hyper")?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects the
    /// `COPY DATABASE` statement — e.g. the source is not attached, the
    /// destination path is not writable, or it already exists.
    pub fn copy_database(&self, source: &str, destination: &str) -> Result<()> {
        let sql = format!(
            "COPY DATABASE {} TO {}",
            escape_sql_path(source),
            escape_sql_path(destination)
        );
        self.execute_command(&sql)?;
        Ok(())
    }

    /// Executes EXPLAIN on a query and returns the plan as a string.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    ///     let plan = conn.explain("SELECT * FROM users WHERE id = 1")?;
    ///     println!("{}", plan);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if `EXPLAIN <query>` fails to parse or
    /// plan, or if the streamed result cannot be consumed.
    pub fn explain(&self, query: &str) -> Result<String> {
        let explain_sql = format!("EXPLAIN {query}");
        let result = self.execute_query(&explain_sql)?;
        let mut lines = Vec::new();
        for row in result.rows() {
            let row = row?;
            if let Some(line) = row.get::<String>(0) {
                lines.push(line);
            }
        }
        Ok(lines.join("\n"))
    }

    /// Executes EXPLAIN ANALYZE on a query and returns the plan with timing info.
    ///
    /// **Note:** This actually executes the query to collect timing information.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if `EXPLAIN ANALYZE <query>` fails — this
    /// includes any runtime error raised by actually executing `query`.
    pub fn explain_analyze(&self, query: &str) -> Result<String> {
        let explain_sql = format!("EXPLAIN ANALYZE {query}");
        let result = self.execute_query(&explain_sql)?;
        let mut lines = Vec::new();
        for row in result.rows() {
            let row = row?;
            if let Some(line) = row.get::<String>(0) {
                lines.push(line);
            }
        }
        Ok(lines.join("\n"))
    }

    /// Returns a reference to the underlying TCP client.
    ///
    /// # Panics
    ///
    /// This method returns `None` if the connection is using gRPC transport.
    pub fn tcp_client(&self) -> Option<&Client> {
        match &self.transport {
            Transport::Tcp(tcp) => Some(&tcp.client),
            Transport::Grpc(_) => None,
        }
    }

    /// Crate-internal accessor for the transport. Used by
    /// [`PreparedStatement`](crate::PreparedStatement) to reach the
    /// underlying `hyperdb_api_core::client::Client`.
    pub(crate) fn transport(&self) -> &Transport {
        &self.transport
    }

    /// Prepares a SQL statement with automatic parameter type inference.
    ///
    /// The returned [`PreparedStatement`](crate::PreparedStatement) can
    /// be executed many times with different parameter values; the
    /// server caches the parsed plan. This is the preferred way to
    /// execute a statement repeatedly inside a loop.
    ///
    /// For explicit parameter types (necessary when `$N` placeholders
    /// would otherwise be ambiguous), use
    /// [`prepare_typed`](Self::prepare_typed).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, CreateMode, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let stmt = conn.prepare("SELECT name FROM users WHERE id = $1")?;
    /// for id in [1_i32, 2, 3] {
    ///     let name: String = stmt.fetch_scalar(&[&id])?;
    ///     println!("{id}: {name}");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// See [`prepare_typed`](Self::prepare_typed) — this method delegates
    /// to it with an empty OID list.
    pub fn prepare(&self, query: &str) -> Result<crate::PreparedStatement<'_>> {
        self.prepare_typed(query, &[])
    }

    /// Prepares a SQL statement with explicit parameter type OIDs.
    ///
    /// Use this when the server cannot infer parameter types from the
    /// SQL alone (e.g. a bare `$1` in a `WHERE v > $1` clause with no
    /// other context). Constants for common types live in
    /// [`hyperdb_api_core::types::oids`].
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] if the connection is using gRPC transport
    ///   (prepared statements are TCP-only).
    /// - Returns [`Error::Server`] if the server rejects the `Parse`
    ///   message, e.g. SQL syntax error or unknown OID.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub fn prepare_typed(
        &self,
        query: &str,
        param_types: &[crate::Oid],
    ) -> Result<crate::PreparedStatement<'_>> {
        let client = match &self.transport {
            Transport::Tcp(tcp) => &tcp.client,
            Transport::Grpc(_) => {
                return Err(Error::feature_not_supported(
                    "prepared statements are not supported over gRPC transport",
                ));
            }
        };
        let inner = client.prepare_typed(query, param_types)?;
        crate::PreparedStatement::new(self, inner)
    }

    /// Returns true if the connection is alive (passive check).
    ///
    /// This is a lightweight check that does not send any data to the server.
    /// For an active health check, use [`ping`](Self::ping).
    pub fn is_alive(&self) -> bool {
        match &self.transport {
            Transport::Tcp(tcp) => tcp.client.is_alive(),
            Transport::Grpc(_) => true, // gRPC connections are stateless
        }
    }

    /// Actively checks that the connection is healthy by executing a trivial query.
    ///
    /// Unlike [`is_alive`](Self::is_alive) which only checks local state,
    /// this method sends `SELECT 1` to the server and verifies a response.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, CreateMode, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// if conn.ping().is_ok() {
    ///     println!("Connection is healthy");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] or [`Error::Io`] if the `SELECT 1`
    /// round-trip fails — i.e. the connection is no longer usable.
    pub fn ping(&self) -> Result<()> {
        self.execute_command("SELECT 1")?;
        Ok(())
    }

    /// Returns the process ID of the backend server connection.
    ///
    /// Returns 0 for gRPC connections (not applicable).
    pub fn process_id(&self) -> i32 {
        match &self.transport {
            Transport::Tcp(tcp) => tcp.client.process_id(),
            Transport::Grpc(_) => 0,
        }
    }

    /// Returns the secret key for the backend server connection.
    ///
    /// This is used for cancellation requests.
    /// Returns 0 for gRPC connections (not applicable).
    pub fn secret_key(&self) -> i32 {
        match &self.transport {
            Transport::Tcp(tcp) => tcp.client.secret_key(),
            Transport::Grpc(_) => 0,
        }
    }

    /// Returns a server parameter value by name.
    ///
    /// Server parameters are sent by the server during connection startup.
    /// Common parameters include:
    /// - `server_version` - The server version string
    /// - `server_encoding` - The server's character encoding
    /// - `client_encoding` - The client's character encoding
    /// - `DateStyle` - Date display format
    /// - `TimeZone` - Server timezone
    /// - `session_identifier` - Session ID for connection migration (if routing enabled)
    ///
    /// Returns `None` if the parameter is not known.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let hyper = HyperProcess::new(None, None)?;
    ///     let conn = Connection::new(&hyper, "test.hyper", CreateMode::CreateIfNotExists)?;
    ///
    ///     if let Some(version) = conn.parameter_status("server_version") {
    ///         println!("Connected to Hyper version: {}", version);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub fn parameter_status(&self, name: &str) -> Option<String> {
        match &self.transport {
            Transport::Tcp(tcp) => tcp.client.parameter_status(name),
            Transport::Grpc(_) => None, // gRPC doesn't have server parameters
        }
    }

    /// Sets the notice receiver for this connection.
    ///
    /// Server notices and warnings are passed to this callback instead of being
    /// logged. Pass `None` to restore default logging behavior.
    pub fn set_notice_receiver(
        &mut self,
        receiver: Option<hyperdb_api_core::client::NoticeReceiver>,
    ) {
        match &mut self.transport {
            Transport::Tcp(tcp) => tcp.client.set_notice_receiver(receiver),
            Transport::Grpc(_) => {} // gRPC doesn't support notice receivers
        }
    }

    /// Cancels the currently executing query (thread-safe).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC connections — cancellation is not
    ///   yet implemented for gRPC transport.
    /// - Returns [`Error::Connection`] or [`Error::Io`] if the separate
    ///   cancel-request connection to the server fails.
    pub fn cancel(&self) -> Result<()> {
        match &self.transport {
            Transport::Tcp(tcp) => tcp.client.cancel().map_err(Error::from),
            Transport::Grpc(_) => Err(Error::feature_not_supported(
                "Query cancellation is not yet supported for gRPC connections.",
            )),
        }
    }

    /// Closes the connection, detaching all databases first.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] wrapping the underlying close failure
    ///   (its `source` is the transport error) if the client cannot be
    ///   shut down cleanly.
    /// - Returns [`Error::Internal`] wrapping the detach failure if the
    ///   attached database could not be detached but close itself
    ///   succeeded.
    pub fn close(self) -> Result<()> {
        // Detach the attached database to ensure files are flushed and released.
        // Always attempt close, even if detach fails.
        let detach_err = if let Some(ref db_path) = self.database {
            let db_alias = std::path::Path::new(db_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("db");
            self.execute_command(&format!("DETACH DATABASE {}", escape_sql_path(db_alias)))
                .err()
        } else {
            None
        };

        // Always attempt to close the client to release the connection.
        let close_result = match self.transport {
            Transport::Tcp(tcp) => tcp.client.close(),
            Transport::Grpc(_) => Ok(()), // gRPC connections are stateless
        };

        if let Err(e) = close_result {
            return Err(Error::internal(format!("Failed to close connection: {e}")));
        }

        if let Some(e) = detach_err {
            // Detach failed but close succeeded; surface the detach error.
            return Err(Error::internal(format!(
                "Failed to detach database during close: {e}"
            )));
        }

        Ok(())
    }

    /// Unloads the database from memory while keeping the connection active.
    ///
    /// This executes the `UNLOAD DATABASE` command, which releases the database
    /// from memory but keeps the session and connection open. The database can
    /// be accessed again by subsequent queries that will automatically reload it.
    ///
    /// This is useful for releasing memory locks when switching between databases
    /// or when working with multiple database files.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let hyper = HyperProcess::new(None, None)?;
    ///     let conn = Connection::new(&hyper, "test.hyper", CreateMode::Create)?;
    ///     
    ///     // Do some work with the database
    ///     conn.execute_command("CREATE TABLE test (id INT)")?;
    ///     
    ///     // Unload from memory (but keep connection)
    ///     conn.unload_database()?;
    ///     
    ///     // Database can still be accessed (will be reloaded automatically)
    ///     let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM test")?;
    ///     println!("Count: {}", count);
    ///     
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects the `UNLOAD DATABASE`
    /// command (e.g. the database is still in use by another session).
    pub fn unload_database(&self) -> Result<()> {
        self.execute_command("UNLOAD DATABASE")?;
        Ok(())
    }

    /// Releases the database completely from the session.
    ///
    /// This executes the `UNLOAD RELEASE` command, which completely releases
    /// the database from the session. After this call, the database cannot
    /// be accessed until a new connection is established.
    ///
    /// This is useful for completely freeing database resources when you're
    /// done with a database and want to ensure no locks are held.
    ///
    /// **Note:** This should only be used when the session has exactly one
    /// database attached. Hyper does not support `UNLOAD RELEASE` with
    /// multiple databases attached to the same session.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let hyper = HyperProcess::new(None, None)?;
    ///     let conn = Connection::new(&hyper, "test.hyper", CreateMode::Create)?;
    ///     
    ///     // Do some work with the database
    ///     conn.execute_command("CREATE TABLE test (id INT)")?;
    ///     
    ///     // Release database completely from session
    ///     conn.unload_release()?;
    ///     
    ///     // Database cannot be accessed after this point without new connection
    ///     // conn.execute_command("SELECT * FROM test")?; // This would fail
    ///     
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects `UNLOAD RELEASE`, most
    /// commonly because multiple databases are attached to the same session
    /// (Hyper only supports `UNLOAD RELEASE` with exactly one attached DB).
    pub fn unload_release(&self) -> Result<()> {
        self.execute_command("UNLOAD RELEASE")?;
        Ok(())
    }

    // =========================================================================
    // Query Statistics
    // =========================================================================

    /// Enables query statistics collection for this connection.
    ///
    /// After enabling, each `execute_command()` or `execute_query()` call will
    /// capture detailed performance metrics from Hyper. Retrieve them via
    /// [`last_query_stats()`](Self::last_query_stats).
    ///
    /// The provider determines how stats are collected. Use
    /// [`LogFileStatsProvider`](crate::LogFileStatsProvider) to parse Hyper's log file (requires local
    /// `hyperd.log`), or implement a custom [`QueryStatsProvider`](crate::QueryStatsProvider).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};
    /// # fn main() -> Result<()> {
    /// # let hyper = HyperProcess::new(None, None)?;
    /// # let mut conn = Connection::new(&hyper, "test.hyper", CreateMode::CreateIfNotExists)?;
    /// use hyperdb_api::LogFileStatsProvider;
    ///
    /// // Auto-detect log path from HyperProcess
    /// conn.enable_query_stats(LogFileStatsProvider::from_process(&hyper));
    ///
    /// // Or specify an explicit log path
    /// // conn.enable_query_stats(LogFileStatsProvider::new("/path/to/hyperd.log"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn enable_query_stats(&mut self, provider: impl QueryStatsProvider + 'static) {
        self.stats_provider = Some(Arc::new(provider));
    }

    /// Disables query statistics collection.
    ///
    /// After calling this, `last_query_stats()` will return `None`.
    pub fn disable_query_stats(&mut self) {
        self.stats_provider = None;
        if let Ok(mut guard) = self.pending_stats.lock() {
            *guard = None;
        }
    }

    /// Returns the query statistics from the most recent query execution.
    ///
    /// Stats are resolved **lazily** — the log file is read when this method
    /// is called, not when the query executes. This is important for streaming
    /// queries (`execute_query`), where Hyper writes the execution stats only
    /// after the result set is fully consumed.
    ///
    /// **Call this after consuming the result set** (e.g., after `collect_rows()`,
    /// iterating all chunks, or dropping the `Rowset`).
    ///
    /// Returns `None` if:
    /// - Query stats collection is not enabled
    /// - No query has been executed yet
    /// - Stats could not be found for the last query (e.g., log entry not matched)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};
    /// # fn main() -> Result<()> {
    /// # let hyper = HyperProcess::new(None, None)?;
    /// # let mut conn = Connection::new(&hyper, "test.hyper", CreateMode::CreateIfNotExists)?;
    /// # use hyperdb_api::LogFileStatsProvider;
    /// # conn.enable_query_stats(LogFileStatsProvider::from_process(&hyper));
    /// conn.execute_command("CREATE TABLE t (id INT)")?;
    ///
    /// if let Some(stats) = conn.last_query_stats() {
    ///     println!("Total: {}s", stats.elapsed_s);
    ///     if let Some(ref pre) = stats.pre_execution {
    ///         println!("  Parse: {:?}s", pre.parsing_time_s);
    ///         println!("  Compile: {:?}s", pre.compilation_time_s);
    ///     }
    ///     if let Some(ref exec) = stats.execution {
    ///         println!("  Execute: {:?}s", exec.elapsed_s);
    ///         println!("  Peak mem: {:?} MB", exec.peak_memory_mb);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn last_query_stats(&self) -> Option<QueryStats> {
        let provider = self.stats_provider.as_ref()?;
        let mut guard = self.pending_stats.lock().ok()?;
        let (token, sql) = guard.take()?;
        provider.after_query(token, &sql)
    }

    /// Internal: call provider's `before_query` if stats are enabled.
    fn stats_before_query(&self, sql: &str) -> Option<Box<dyn Any + Send>> {
        self.stats_provider.as_ref().map(|p| p.before_query(sql))
    }

    /// Internal: store the pending token+sql for lazy resolution.
    fn stats_store_pending(&self, token: Option<Box<dyn Any + Send>>, sql: &str) {
        if let Some(token) = token {
            if let Ok(mut guard) = self.pending_stats.lock() {
                *guard = Some((token, sql.to_string()));
            }
        }
    }
}

impl Connection {
    // =========================================================================
    // Transaction Control
    // =========================================================================

    // -------------------------------------------------------------------
    // Raw transaction control (internal)
    // -------------------------------------------------------------------
    //
    // The `*_raw` methods below are `pub(crate)` and form the canonical
    // implementation of session-level transaction control. The RAII
    // guard at `crate::Transaction` and any internal helper that
    // genuinely needs `&self` (rather than the guard's `&mut self`)
    // delegate to these.
    //
    // The matching `pub` methods (`begin_transaction`, `commit`,
    // `rollback`) are thin `#[doc(hidden)] #[deprecated]` wrappers
    // retained only so any pre-existing downstream caller sees a
    // compiler warning rather than a hard break. They will be deleted
    // in a future release; the `_raw` methods stay.

    /// Issues `BEGIN TRANSACTION`. Crate-internal use only.
    pub(crate) fn begin_transaction_raw(&self) -> Result<()> {
        self.execute_command("BEGIN TRANSACTION")?;
        Ok(())
    }

    /// Issues `COMMIT`. Crate-internal use only.
    pub(crate) fn commit_raw(&self) -> Result<()> {
        self.execute_command("COMMIT")?;
        Ok(())
    }

    /// Issues `ROLLBACK`. Crate-internal use only.
    pub(crate) fn rollback_raw(&self) -> Result<()> {
        self.execute_command("ROLLBACK")?;
        Ok(())
    }

    /// Begins an explicit transaction.
    ///
    /// **Prefer [`transaction()`](Self::transaction)** — the RAII guard
    /// auto-rolls back on drop and cannot leak a half-open transaction
    /// across error paths. This method is hidden from generated
    /// rustdoc and marked deprecated; it will be removed in a future
    /// release.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects `BEGIN TRANSACTION`
    /// (e.g. a transaction is already open on this session).
    #[doc(hidden)]
    #[deprecated(
        note = "Use `Connection::transaction()` for an RAII guard. This method will be removed \
                in a future release."
    )]
    pub fn begin_transaction(&self) -> Result<()> {
        self.begin_transaction_raw()
    }

    /// Commits the current transaction.
    ///
    /// **Prefer [`Transaction::commit`](crate::Transaction::commit)** on
    /// the RAII guard returned by [`transaction()`](Self::transaction).
    /// Hidden from generated rustdoc and deprecated; slated for removal.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects `COMMIT`.
    #[doc(hidden)]
    #[deprecated(
        note = "Use `Transaction::commit()` on the RAII guard from `Connection::transaction()`. \
                This method will be removed in a future release."
    )]
    pub fn commit(&self) -> Result<()> {
        self.commit_raw()
    }

    /// Rolls back the current transaction.
    ///
    /// **Prefer [`Transaction::rollback`](crate::Transaction::rollback)**
    /// on the RAII guard returned by [`transaction()`](Self::transaction).
    /// Hidden from generated rustdoc and deprecated; slated for removal.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects `ROLLBACK`.
    #[doc(hidden)]
    #[deprecated(
        note = "Use `Transaction::rollback()` on the RAII guard from `Connection::transaction()`. \
                This method will be removed in a future release."
    )]
    pub fn rollback(&self) -> Result<()> {
        self.rollback_raw()
    }

    /// Starts a transaction and returns an RAII guard that auto-rolls back on drop.
    ///
    /// The returned [`Transaction`](crate::Transaction) exclusively borrows this connection,
    /// preventing any other use of the connection while the transaction is active.
    /// This is enforced at compile time by Rust's borrow checker. The guard provides
    /// `commit()` and `rollback()` methods. If dropped without calling either, the
    /// transaction is automatically rolled back.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, CreateMode, Result};
    /// # fn main() -> Result<()> {
    /// # let mut conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
    /// let txn = conn.transaction()?;
    /// txn.execute_command("INSERT INTO users VALUES (1, 'Alice')")?;
    /// txn.commit()?; // or drop `txn` to auto-rollback
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects the `BEGIN`
    /// statement issued internally by
    /// [`Transaction::new`](crate::Transaction).
    pub fn transaction(&mut self) -> Result<crate::Transaction<'_>> {
        crate::Transaction::new(self)
    }
}

/// Checks if an error indicates an "already exists" condition based on SQLSTATE codes.
///
/// This function uses `PostgreSQL` SQLSTATE codes to reliably detect duplicate object errors
/// regardless of server locale or message formatting. The codes checked are:
/// - `42P04`: Database already exists
/// - `42710`: Duplicate object
/// - `42P06`: Duplicate schema
/// - `42P07`: Duplicate table
///
/// See: <https://www.postgresql.org/docs/current/errcodes-appendix.html>
fn is_already_exists_error(err: &Error) -> bool {
    err.sqlstate()
        .is_some_and(|code| matches!(code, "42P04" | "42710" | "42P06" | "42P07"))
}
