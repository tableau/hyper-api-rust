// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async connection to Hyper database.
//!
//! This module provides [`AsyncConnection`] the async version of [`Connection`](crate::Connection).
//! Use this when you're already in an async runtime (tokio).

use std::any::Any;
use std::sync::{Arc, Mutex};

use crate::async_result::AsyncRowset;
use crate::async_transport::{AsyncTcpTransport, AsyncTransport};
use crate::error::{Error, Result};
use crate::names::escape_sql_path;
use crate::query_stats::{QueryStats, QueryStatsProvider};
use crate::result::{Row, RowValue};
use crate::CreateMode;

/// An async connection to a Hyper database.
///
/// This is the async equivalent of [`Connection`](crate::Connection), designed for use
/// in tokio-based async applications. All I/O operations are non-blocking.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{AsyncConnection, CreateMode, Result};
///
/// #[tokio::main]
/// async fn main() -> Result<()> {
///     let conn = AsyncConnection::connect(
///         "localhost:7483",
///         "example.hyper",
///         CreateMode::CreateIfNotExists,
///     ).await?;
///
///     conn.execute_command("CREATE TABLE test (id INT)").await?;
///     let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM test").await?;
///
///     conn.close().await?;
///     Ok(())
/// }
/// ```
pub struct AsyncConnection {
    transport: AsyncTransport,
    database: Option<String>,
    stats_provider: Mutex<Option<Arc<dyn QueryStatsProvider>>>,
    pending_stats: Mutex<Option<(Box<dyn Any + Send>, String)>>,
}

impl std::fmt::Debug for AsyncConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncConnection")
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

impl AsyncConnection {
    /// Returns a fluent [`AsyncConnectionBuilder`](crate::AsyncConnectionBuilder)
    /// pointed at `endpoint`.
    #[must_use]
    pub fn builder(endpoint: &str) -> crate::AsyncConnectionBuilder {
        crate::AsyncConnectionBuilder::new(endpoint)
    }

    /// Connects to a Hyper server (async).
    ///
    /// Transport is auto-detected from the endpoint:
    /// - `https://` or `http://` → gRPC transport
    /// - Otherwise → TCP transport (`PostgreSQL` wire protocol)
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Io`] / [`Error::Connection`] if the handshake with
    ///   the server fails.
    /// - Returns [`Error::Server`] if the `CreateMode` SQL (`CREATE`
    ///   / `DROP` / `ATTACH`) is rejected by the server.
    pub async fn connect(endpoint: &str, database: &str, mode: CreateMode) -> Result<Self> {
        let transport = AsyncTransport::connect(endpoint, Some(database)).await?;
        let conn = AsyncConnection {
            transport,
            database: Some(database.to_string()),
            stats_provider: Mutex::new(None),
            pending_stats: Mutex::new(None),
        };

        if conn.transport.supports_writes() {
            conn.handle_creation_mode(database, mode).await?;
            conn.attach_and_set_path(database).await?;
        }

        Ok(conn)
    }

    /// Connects with authentication (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Authentication`] if authentication is rejected.
    /// - Returns [`Error::Io`] if the endpoint cannot be reached.
    /// - Returns [`Error::Server`] if the `CreateMode` SQL is rejected.
    pub async fn connect_with_auth(
        endpoint: &str,
        database: &str,
        mode: CreateMode,
        user: &str,
        password: &str,
    ) -> Result<Self> {
        let transport = AsyncTransport::connect_tcp_with_auth(endpoint, user, password).await?;
        let conn = AsyncConnection {
            transport,
            database: Some(database.to_string()),
            stats_provider: Mutex::new(None),
            pending_stats: Mutex::new(None),
        };

        conn.handle_creation_mode(database, mode).await?;
        conn.attach_and_set_path(database).await?;

        Ok(conn)
    }

    /// Connects to a server without attaching any database (async).
    ///
    /// Useful for running `CREATE DATABASE` / `DROP DATABASE` without an
    /// active attachment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] or [`Error::Connection`] if the TCP handshake
    /// with `endpoint` fails.
    pub async fn without_database(endpoint: &str) -> Result<Self> {
        let transport = AsyncTransport::connect_tcp(endpoint).await?;
        Ok(AsyncConnection {
            transport,
            database: None,
            stats_provider: Mutex::new(None),
            pending_stats: Mutex::new(None),
        })
    }

    /// Builds an `AsyncConnection` from a pre-existing `AsyncClient` (TCP only).
    #[must_use]
    pub fn from_async_client(
        client: hyperdb_api_core::client::AsyncClient,
        database: Option<String>,
    ) -> Self {
        AsyncConnection {
            transport: AsyncTransport::Tcp(AsyncTcpTransport { client }),
            database,
            stats_provider: Mutex::new(None),
            pending_stats: Mutex::new(None),
        }
    }

    /// Builds an `AsyncConnection` from a pre-constructed transport.
    ///
    /// Used by [`AsyncConnectionBuilder`](crate::AsyncConnectionBuilder) to
    /// stitch together a gRPC transport after its own config construction.
    pub(crate) fn from_transport(transport: AsyncTransport, database: Option<String>) -> Self {
        AsyncConnection {
            transport,
            database,
            stats_provider: Mutex::new(None),
            pending_stats: Mutex::new(None),
        }
    }

    /// Runs the configured `CreateMode` as SQL (crate-public for use by
    /// [`AsyncConnectionBuilder`](crate::AsyncConnectionBuilder)).
    pub(crate) async fn handle_creation_mode_public(
        &self,
        database: &str,
        mode: CreateMode,
    ) -> Result<()> {
        self.handle_creation_mode(database, mode).await
    }

    /// Attaches the database and sets `search_path` (crate-public for use
    /// by [`AsyncConnectionBuilder`](crate::AsyncConnectionBuilder)).
    pub(crate) async fn attach_and_set_path_public(&self, database: &str) -> Result<()> {
        self.attach_and_set_path(database).await
    }

    async fn handle_creation_mode(&self, database: &str, mode: CreateMode) -> Result<()> {
        let escaped_db = escape_sql_path(database);
        match mode {
            CreateMode::Create => {
                self.execute_command(&format!("CREATE DATABASE {escaped_db}"))
                    .await?;
            }
            CreateMode::CreateIfNotExists => {
                self.execute_command(&format!("CREATE DATABASE IF NOT EXISTS {escaped_db}"))
                    .await?;
            }
            CreateMode::CreateAndReplace => {
                self.execute_command(&format!("DROP DATABASE IF EXISTS {escaped_db}"))
                    .await?;
                self.execute_command(&format!("CREATE DATABASE {escaped_db}"))
                    .await?;
            }
            CreateMode::DoNotCreate => {}
        }
        Ok(())
    }

    async fn attach_and_set_path(&self, database: &str) -> Result<()> {
        let escaped_db = escape_sql_path(database);
        let db_alias = std::path::Path::new(database)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("db");
        let escaped_alias = escape_sql_path(db_alias);

        self.execute_command(&format!("ATTACH DATABASE {escaped_db} AS {escaped_alias}"))
            .await?;

        self.execute_command(&format!("SET search_path TO {escaped_alias}, public"))
            .await?;
        Ok(())
    }

    /// Returns the transport type name (e.g., "TCP", "gRPC").
    pub fn transport_type(&self) -> &'static str {
        self.transport.transport_type().as_str()
    }

    /// Returns true if this connection supports write operations.
    pub fn supports_writes(&self) -> bool {
        self.transport.supports_writes()
    }

    /// Returns the database path.
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    // =========================================================================
    // Command Execution
    // =========================================================================

    /// Executes a SQL command that doesn't return rows (async).
    ///
    /// Use for DDL statements (CREATE, DROP, ALTER) and DML statements
    /// (INSERT, UPDATE, DELETE). Returns the number of affected rows (DML)
    /// or 0 (DDL).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transports that do not yet
    ///   support write operations.
    /// - Returns [`Error::Server`] if the SQL fails to parse or execute.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub async fn execute_command(&self, sql: &str) -> Result<u64> {
        let token = self.stats_before_query(sql);
        let result = self.transport.execute_command(sql).await;
        self.stats_store_pending(token, sql);
        result
    }

    /// Executes multiple SQL statements sequentially (async).
    ///
    /// If any statement fails, execution stops and the error is returned
    /// wrapping the SQL preview for context.
    ///
    /// # Errors
    ///
    /// Returns an [`Error::Internal`] wrapping the first failing statement's
    /// error; the wrapping message includes the statement's ordinal and
    /// an 80-character SQL preview.
    pub async fn execute_batch(&self, statements: &[&str]) -> Result<u64> {
        let mut total = 0u64;
        for (i, stmt) in statements.iter().enumerate() {
            if !stmt.trim().is_empty() {
                total += self.execute_command(stmt).await.map_err(|e| {
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

    // =========================================================================
    // Query Execution (Streaming)
    // =========================================================================

    /// Executes a SQL query and returns a streaming [`AsyncRowset`] (async).
    ///
    /// Results are streamed in chunks so memory usage stays constant
    /// regardless of result set size. See [`AsyncRowset`] for the row-level
    /// API and collectors.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Server`] if the SQL is rejected by the server.
    /// - Returns [`Error::Io`] on transport-level I/O failures while
    ///   opening the stream.
    pub async fn execute_query(&self, query: &str) -> Result<AsyncRowset<'_>> {
        let token = self.stats_before_query(query);
        let result = self.transport.execute_query_streaming(query).await;
        self.stats_store_pending(token, query);
        result
    }

    /// Fetches a single row, erroring if the query returns zero rows.
    ///
    /// # Errors
    ///
    /// - Returns the error from [`execute_query`](Self::execute_query) if
    ///   the query fails.
    /// - Returns [`Error::Conversion`] with message `"Query returned no rows"` if
    ///   the query produced zero rows.
    pub async fn fetch_one<Q: AsRef<str>>(&self, query: Q) -> Result<Row> {
        self.execute_query(query.as_ref())
            .await?
            .require_first_row()
            .await
    }

    /// Fetches a single row, returning `None` if the query is empty.
    ///
    /// # Errors
    ///
    /// Returns the error from [`execute_query`](Self::execute_query) if the
    /// query fails. An empty result set yields `Ok(None)`, not an error.
    pub async fn fetch_optional<Q: AsRef<str>>(&self, query: Q) -> Result<Option<Row>> {
        self.execute_query(query.as_ref()).await?.first_row().await
    }

    /// Fetches all rows from a query.
    ///
    /// # Errors
    ///
    /// Returns the error from [`execute_query`](Self::execute_query), or a
    /// transport error produced while draining every chunk.
    pub async fn fetch_all<Q: AsRef<str>>(&self, query: Q) -> Result<Vec<Row>> {
        self.execute_query(query.as_ref())
            .await?
            .collect_rows()
            .await
    }

    /// Fetches a single row and maps it to a struct using [`crate::FromRow`].
    ///
    /// # Errors
    ///
    /// - Returns the error from [`fetch_one`](Self::fetch_one).
    /// - Returns whatever [`FromRow::from_row`](crate::FromRow::from_row)
    ///   produces when the row cannot be mapped.
    pub async fn fetch_one_as<T: crate::FromRow>(&self, query: &str) -> Result<T> {
        let row = self.fetch_one(query).await?;
        let indices = row
            .schema()
            .map(crate::row_accessor::RowAccessor::build_indices)
            .unwrap_or_default();
        T::from_row(crate::RowAccessor::new(&row, &indices))
    }

    /// Fetches all rows and maps them to structs using [`crate::FromRow`].
    ///
    /// # Errors
    ///
    /// - Returns the error from [`fetch_all`](Self::fetch_all).
    /// - Returns the first error produced by
    ///   [`FromRow::from_row`](crate::FromRow::from_row) on any row.
    pub async fn fetch_all_as<T: crate::FromRow>(&self, query: &str) -> Result<Vec<T>> {
        let rows = self.fetch_all(query).await?;
        // Build the column-name → index lookup once from the first
        // row's schema; reuse for every row.
        let indices = rows
            .first()
            .and_then(crate::result::Row::schema)
            .map(crate::row_accessor::RowAccessor::build_indices)
            .unwrap_or_default();
        rows.iter()
            .map(|r| T::from_row(crate::RowAccessor::new(r, &indices)))
            .collect()
    }

    /// Fetches a single non-NULL scalar value. Errors on empty / NULL.
    ///
    /// # Errors
    ///
    /// - Returns the error from [`execute_query`](Self::execute_query).
    /// - Returns [`Error::Conversion`] with message `"Query returned no rows"` if
    ///   the query is empty.
    /// - Returns [`Error::Conversion`] with message `"Scalar query returned NULL"`
    ///   if the first cell is SQL `NULL`.
    pub async fn fetch_scalar<T, Q>(&self, query: Q) -> Result<T>
    where
        T: RowValue,
        Q: AsRef<str>,
    {
        self.execute_query(query.as_ref())
            .await?
            .require_scalar()
            .await
    }

    /// Fetches a single scalar value, allowing NULL (returns `None`).
    ///
    /// # Errors
    ///
    /// Returns the error from [`execute_query`](Self::execute_query). An
    /// empty result still yields an error; SQL `NULL` in the first cell
    /// yields `Ok(None)`.
    pub async fn fetch_optional_scalar<T, Q>(&self, query: Q) -> Result<Option<T>>
    where
        T: RowValue,
        Q: AsRef<str>,
    {
        self.execute_query(query.as_ref()).await?.scalar().await
    }

    /// Returns the count from a `SELECT COUNT(*)` style query, defaulting
    /// to 0 on NULL.
    ///
    /// # Errors
    ///
    /// Returns the error from [`execute_query`](Self::execute_query) if the
    /// query itself fails.
    pub async fn query_count(&self, query: &str) -> Result<i64> {
        let opt: Option<i64> = self.fetch_optional_scalar(query).await?;
        Ok(opt.unwrap_or(0))
    }

    // =========================================================================
    // Arrow Queries
    // =========================================================================

    /// Executes a SELECT query and returns results as Arrow IPC stream bytes (async).
    ///
    /// TCP uses `COPY ... TO STDOUT WITH (FORMAT ARROWSTREAM)`; gRPC uses
    /// the native Arrow transport. Both return the same IPC stream shape.
    ///
    /// # Errors
    ///
    /// Propagates any [`Error::Server`] from the transport when the query
    /// fails or the server cannot produce Arrow IPC output.
    pub async fn execute_query_to_arrow(&self, sql: &str) -> Result<bytes::Bytes> {
        self.transport.execute_query_to_arrow(sql).await
    }

    /// Exports an entire table to Arrow IPC stream format (async).
    ///
    /// # Errors
    ///
    /// See [`execute_query_to_arrow`](Self::execute_query_to_arrow).
    pub async fn export_table_to_arrow(&self, table_name: &str) -> Result<bytes::Bytes> {
        self.execute_query_to_arrow(&format!("SELECT * FROM {table_name}"))
            .await
    }

    /// Executes a SELECT query and returns parsed Arrow `RecordBatch`es (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Server`] if the query fails.
    /// - Returns [`Error::Conversion`] if the Arrow IPC payload cannot be
    ///   decoded into record batches.
    pub async fn execute_query_to_batches(
        &self,
        sql: &str,
    ) -> Result<Vec<arrow::record_batch::RecordBatch>> {
        let arrow_data = self.execute_query_to_arrow(sql).await?;
        crate::arrow_result::parse_arrow_ipc(arrow_data)
    }

    // =========================================================================
    // Parameterized Queries
    // =========================================================================

    /// Executes a parameterized query with safely escaped parameters (async).
    ///
    /// Mirrors the sync [`Connection::query_params`](crate::Connection::query_params);
    /// see that method for the design rationale around text-mode escaping
    /// vs. future native Bind/Execute support.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transports (prepared statements
    ///   are TCP-only).
    /// - Returns [`Error::Server`] if the server rejects the statement at
    ///   `Parse`, `Bind`, or `Execute` time.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub async fn query_params(
        &self,
        query: &str,
        params: &[&dyn crate::params::ToSqlParam],
    ) -> Result<AsyncRowset<'_>> {
        // Route through the extended query protocol. See
        // [`Connection::query_params`] for the sync equivalent and the
        // rationale behind the statement-guard pattern.
        let client = match &self.transport {
            AsyncTransport::Tcp(tcp) => &tcp.client,
            AsyncTransport::Grpc(_) => {
                return Err(Error::feature_not_supported(
                    "prepared statements are not supported over gRPC transport",
                ));
            }
        };
        let oids: Vec<crate::Oid> = params.iter().map(|p| p.sql_oid()).collect();
        let stmt = client.prepare_typed(query, &oids).await?;
        let encoded: Vec<Option<Vec<u8>>> = params.iter().map(|p| p.encode_param()).collect();
        let stream = client
            .execute_prepared_streaming(&stmt, encoded, crate::result::DEFAULT_BINARY_CHUNK_SIZE)
            .await?;
        Ok(AsyncRowset::from_prepared(stream).with_statement_guard(stmt))
    }

    /// Executes a parameterized command (INSERT / UPDATE / DELETE) with
    /// binary-encoded parameters via Parse/Bind/Execute (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transports.
    /// - Returns [`Error::Server`] if the server rejects the statement at
    ///   `Parse`, `Bind`, or `Execute` time.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub async fn command_params(
        &self,
        query: &str,
        params: &[&dyn crate::params::ToSqlParam],
    ) -> Result<u64> {
        let client = match &self.transport {
            AsyncTransport::Tcp(tcp) => &tcp.client,
            AsyncTransport::Grpc(_) => {
                return Err(Error::feature_not_supported(
                    "prepared statements are not supported over gRPC transport",
                ));
            }
        };
        let oids: Vec<crate::Oid> = params.iter().map(|p| p.sql_oid()).collect();
        let stmt = client.prepare_typed(query, &oids).await?;
        let encoded: Vec<Option<Vec<u8>>> = params.iter().map(|p| p.encode_param()).collect();
        Ok(client.execute_prepared_no_result(&stmt, encoded).await?)
    }

    // =========================================================================
    // Catalog / Database Management
    // =========================================================================

    /// Creates a new database file (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects
    /// `CREATE DATABASE IF NOT EXISTS` (e.g. the path is not writable).
    pub async fn create_database(&self, path: &str) -> Result<()> {
        let sql = format!("CREATE DATABASE IF NOT EXISTS {}", escape_sql_path(path));
        self.execute_command(&sql).await?;
        Ok(())
    }

    /// Drops (deletes) a database file (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects
    /// `DROP DATABASE IF EXISTS` (e.g. the database is still attached).
    pub async fn drop_database(&self, path: &str) -> Result<()> {
        let sql = format!("DROP DATABASE IF EXISTS {}", escape_sql_path(path));
        self.execute_command(&sql).await?;
        Ok(())
    }

    /// Attaches a database file to the connection (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects the
    /// `ATTACH DATABASE` statement (file missing, permission denied,
    /// alias conflict).
    pub async fn attach_database(&self, path: &str, alias: Option<&str>) -> Result<()> {
        let sql = if let Some(alias) = alias {
            format!(
                "ATTACH DATABASE {} AS {}",
                escape_sql_path(path),
                escape_sql_path(alias)
            )
        } else {
            format!("ATTACH DATABASE {}", escape_sql_path(path))
        };
        self.execute_command(&sql).await?;
        Ok(())
    }

    /// Detaches a database alias from this connection (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the alias is not attached or the
    /// server cannot flush pending updates.
    pub async fn detach_database(&self, alias: &str) -> Result<()> {
        let sql = format!("DETACH DATABASE {}", escape_sql_path(alias));
        self.execute_command(&sql).await?;
        Ok(())
    }

    /// Detaches all databases from this connection (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects
    /// `DETACH ALL DATABASES`.
    pub async fn detach_all_databases(&self) -> Result<()> {
        self.execute_command("DETACH ALL DATABASES").await?;
        Ok(())
    }

    /// Copies a database file to a new path (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects the
    /// `COPY DATABASE` statement — e.g. the source is not attached or the
    /// destination path is not writable.
    pub async fn copy_database(&self, source: &str, destination: &str) -> Result<()> {
        let sql = format!(
            "COPY DATABASE {} TO {}",
            escape_sql_path(source),
            escape_sql_path(destination)
        );
        self.execute_command(&sql).await?;
        Ok(())
    }

    /// Creates a schema in the database (async).
    ///
    /// # Errors
    ///
    /// - Returns an error if `schema_name` cannot be converted to a
    ///   [`SchemaName`](crate::SchemaName).
    /// - Returns [`Error::Server`] if the server rejects
    ///   `CREATE SCHEMA IF NOT EXISTS`.
    pub async fn create_schema<T>(&self, schema_name: T) -> Result<()>
    where
        T: TryInto<crate::SchemaName>,
        crate::Error: From<T::Error>,
    {
        let schema: crate::SchemaName = schema_name.try_into()?;
        let sql = format!("CREATE SCHEMA IF NOT EXISTS {schema}");
        self.execute_command(&sql).await?;
        Ok(())
    }

    /// Checks whether a schema exists (async).
    ///
    /// # Errors
    ///
    /// - Returns an error if `schema` cannot be converted to a
    ///   [`SchemaName`](crate::SchemaName).
    /// - Returns [`Error::Server`] if the catalog lookup query fails.
    pub async fn has_schema<T>(&self, schema: T) -> Result<bool>
    where
        T: TryInto<crate::SchemaName>,
        crate::Error: From<T::Error>,
    {
        let schema: crate::SchemaName = schema.try_into()?;
        let db_prefix = if let Some(db) = schema.database() {
            format!("{db}.")
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT 1 FROM {}pg_catalog.pg_namespace WHERE nspname = '{}'",
            db_prefix,
            schema.unescaped().replace('\'', "''")
        );
        Ok(self.fetch_optional(&sql).await?.is_some())
    }

    /// Checks whether a table exists (async).
    ///
    /// # Errors
    ///
    /// - Returns an error if `table_name` cannot be converted to a
    ///   [`TableName`](crate::TableName).
    /// - Returns [`Error::Server`] if the catalog lookup query fails.
    pub async fn has_table<T>(&self, table_name: T) -> Result<bool>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let table: crate::TableName = table_name.try_into()?;
        let schema = table
            .schema()
            .map_or("public", super::names::Name::unescaped);
        let db_prefix = if let Some(db) = table.database() {
            format!("{db}.")
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT 1 FROM {}pg_catalog.pg_tables WHERE schemaname = '{}' AND tablename = '{}'",
            db_prefix,
            schema.replace('\'', "''"),
            table.table().unescaped().replace('\'', "''")
        );
        Ok(self.fetch_optional(&sql).await?.is_some())
    }

    /// Unloads the database from memory but keeps the session alive (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects `UNLOAD DATABASE`
    /// (e.g. the database is in use by another session).
    pub async fn unload_database(&self) -> Result<()> {
        self.execute_command("UNLOAD DATABASE").await?;
        Ok(())
    }

    /// Releases the database completely from the session (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects `UNLOAD RELEASE`,
    /// most commonly because multiple databases are attached to the same
    /// session.
    pub async fn unload_release(&self) -> Result<()> {
        self.execute_command("UNLOAD RELEASE").await?;
        Ok(())
    }

    // =========================================================================
    // Diagnostics / Explain
    // =========================================================================

    /// Executes EXPLAIN and returns the plan text (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if `EXPLAIN <query>` fails to parse or plan.
    pub async fn explain(&self, query: &str) -> Result<String> {
        let sql = format!("EXPLAIN {query}");
        let rows = self.fetch_all(&sql).await?;
        let lines: Vec<String> = rows.iter().filter_map(|r| r.get::<String>(0)).collect();
        Ok(lines.join("\n"))
    }

    /// Executes EXPLAIN ANALYZE and returns the plan with timing (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if `EXPLAIN ANALYZE <query>` fails — this
    /// includes any runtime error raised by actually executing `query`.
    pub async fn explain_analyze(&self, query: &str) -> Result<String> {
        let sql = format!("EXPLAIN ANALYZE {query}");
        let rows = self.fetch_all(&sql).await?;
        let lines: Vec<String> = rows.iter().filter_map(|r| r.get::<String>(0)).collect();
        Ok(lines.join("\n"))
    }

    // =========================================================================
    // Connection Introspection / Lifecycle
    // =========================================================================

    /// Returns true if the connection is alive (passive check).
    pub fn is_alive(&self) -> bool {
        match &self.transport {
            AsyncTransport::Tcp(tcp) => tcp.client.is_alive(),
            AsyncTransport::Grpc(_) => true,
        }
    }

    /// Actively pings the server with `SELECT 1` (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] or [`Error::Io`] if the `SELECT 1`
    /// round-trip fails — i.e. the connection is no longer usable.
    pub async fn ping(&self) -> Result<()> {
        self.execute_command("SELECT 1").await?;
        Ok(())
    }

    /// Returns the backend process ID, or 0 for gRPC transports.
    pub fn process_id(&self) -> i32 {
        match &self.transport {
            AsyncTransport::Tcp(tcp) => tcp.client.process_id(),
            AsyncTransport::Grpc(_) => 0,
        }
    }

    /// Returns the secret key used for cancel requests, or 0 for gRPC.
    pub fn secret_key(&self) -> i32 {
        match &self.transport {
            AsyncTransport::Tcp(tcp) => tcp.client.secret_key(),
            AsyncTransport::Grpc(_) => 0,
        }
    }

    /// Returns a server parameter value by name (async).
    pub async fn parameter_status(&self, name: &str) -> Option<String> {
        match &self.transport {
            AsyncTransport::Tcp(tcp) => tcp.client.parameter_status(name).await,
            AsyncTransport::Grpc(_) => None,
        }
    }

    /// Returns the server version as a parsed struct (async).
    pub async fn server_version(&self) -> Option<crate::ServerVersion> {
        let version_str = self.parameter_status("server_version").await?;
        crate::ServerVersion::parse(&version_str)
    }

    /// Sets the notice receiver callback for this connection.
    pub fn set_notice_receiver(
        &mut self,
        receiver: Option<Box<dyn Fn(hyperdb_api_core::client::Notice) + Send + Sync>>,
    ) {
        match &mut self.transport {
            AsyncTransport::Tcp(tcp) => tcp.client.set_notice_receiver(receiver),
            AsyncTransport::Grpc(_) => {}
        }
    }

    /// Cancels the currently running query (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transports — cancellation is not
    ///   yet implemented for gRPC.
    /// - Returns [`Error::Connection`] or [`Error::Io`] if the cancel-request
    ///   connection to the server fails.
    pub async fn cancel(&self) -> Result<()> {
        self.transport.cancel().await
    }

    /// Closes the connection gracefully, detaching any attached database first (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Internal`] wrapping the transport close failure if
    ///   the client cannot be shut down cleanly.
    /// - Returns [`Error::Internal`] wrapping the detach failure if the
    ///   attached database could not be detached but the transport close
    ///   itself succeeded.
    pub async fn close(self) -> Result<()> {
        let detach_err = if let Some(ref db_path) = self.database {
            let db_alias = std::path::Path::new(db_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("db");
            self.execute_command(&format!("DETACH DATABASE {}", escape_sql_path(db_alias)))
                .await
                .err()
        } else {
            None
        };

        let close_result = self.transport.close().await;

        if let Err(e) = close_result {
            return Err(Error::internal(format!(
                "Failed to close async connection: {e}"
            )));
        }

        if let Some(e) = detach_err {
            return Err(Error::internal(format!(
                "Failed to detach database during close: {e}"
            )));
        }

        Ok(())
    }

    /// Returns a reference to the underlying async TCP client (`None` for gRPC).
    ///
    /// Prefer the high-level `AsyncConnection` methods; this escape hatch
    /// remains for code that needs direct protocol access (e.g. custom
    /// COPY loops).
    pub fn async_tcp_client(&self) -> Option<&hyperdb_api_core::client::AsyncClient> {
        self.transport.async_tcp_client()
    }

    /// Crate-internal accessor for the transport. Used by
    /// [`AsyncPreparedStatement`](crate::AsyncPreparedStatement) to reach
    /// the underlying `hyperdb_api_core::client::AsyncClient`.
    pub(crate) fn transport(&self) -> &AsyncTransport {
        &self.transport
    }

    /// Prepares a SQL statement (async).
    ///
    /// See [`Connection::prepare`](crate::Connection::prepare) for
    /// semantics. The returned
    /// [`AsyncPreparedStatement`](crate::AsyncPreparedStatement) can be
    /// executed many times with different parameter values.
    ///
    /// # Errors
    ///
    /// See [`prepare_typed`](Self::prepare_typed) — this method delegates
    /// to it with an empty OID list.
    pub async fn prepare(&self, query: &str) -> Result<crate::AsyncPreparedStatement<'_>> {
        self.prepare_typed(query, &[]).await
    }

    /// Prepares a SQL statement with explicit parameter type OIDs (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transports (prepared statements
    ///   are TCP-only).
    /// - Returns [`Error::Server`] if the server rejects the `Parse`
    ///   message (SQL syntax error, unknown OID).
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub async fn prepare_typed(
        &self,
        query: &str,
        param_types: &[crate::Oid],
    ) -> Result<crate::AsyncPreparedStatement<'_>> {
        let client = match &self.transport {
            AsyncTransport::Tcp(tcp) => &tcp.client,
            AsyncTransport::Grpc(_) => {
                return Err(Error::feature_not_supported(
                    "prepared statements are not supported over gRPC transport",
                ));
            }
        };
        let inner = client.prepare_typed(query, param_types).await?;
        crate::AsyncPreparedStatement::new(self, inner)
    }

    /// Owned-handle variant of [`prepare`](Self::prepare). Returns a
    /// `'static`-lifetime [`AsyncPreparedStatementOwned`](crate::AsyncPreparedStatementOwned)
    /// that holds an `Arc`-cloned reference to `self`.
    ///
    /// Intended for N-API consumers and any other caller that needs
    /// the prepared statement to outlive the stack frame where the
    /// connection is held.
    ///
    /// # Errors
    ///
    /// See [`prepare_typed_arc`](Self::prepare_typed_arc).
    pub async fn prepare_arc(
        self: &Arc<Self>,
        query: &str,
    ) -> Result<crate::async_prepared::AsyncPreparedStatementOwned> {
        self.prepare_typed_arc(query, &[]).await
    }

    /// Owned-handle variant of [`prepare_typed`](Self::prepare_typed).
    ///
    /// # Errors
    ///
    /// - Returns [`Error::FeatureNotSupported`] on gRPC transports.
    /// - Returns [`Error::Server`] if the server rejects the `Parse`
    ///   message.
    /// - Returns [`Error::Io`] on transport-level I/O failures.
    pub async fn prepare_typed_arc(
        self: &Arc<Self>,
        query: &str,
        param_types: &[crate::Oid],
    ) -> Result<crate::async_prepared::AsyncPreparedStatementOwned> {
        let client = match &self.transport {
            AsyncTransport::Tcp(tcp) => &tcp.client,
            AsyncTransport::Grpc(_) => {
                return Err(Error::feature_not_supported(
                    "prepared statements are not supported over gRPC transport",
                ));
            }
        };
        let inner = client.prepare_typed(query, param_types).await?;
        crate::async_prepared::AsyncPreparedStatementOwned::new(Arc::clone(self), inner)
    }

    // =========================================================================
    // Query Statistics
    // =========================================================================

    /// Enables query statistics collection for this connection.
    pub fn enable_query_stats(&self, provider: impl QueryStatsProvider + 'static) {
        if let Ok(mut guard) = self.stats_provider.lock() {
            *guard = Some(Arc::new(provider));
        }
    }

    /// Disables query statistics collection.
    pub fn disable_query_stats(&self) {
        if let Ok(mut guard) = self.stats_provider.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.pending_stats.lock() {
            *guard = None;
        }
    }

    /// Returns the stats for the most recent query (if enabled).
    pub fn last_query_stats(&self) -> Option<QueryStats> {
        let provider = self.stats_provider.lock().ok()?.as_ref().cloned()?;
        let mut guard = self.pending_stats.lock().ok()?;
        let (token, sql) = guard.take()?;
        provider.after_query(token, &sql)
    }

    fn stats_before_query(&self, sql: &str) -> Option<Box<dyn Any + Send>> {
        self.stats_provider
            .lock()
            .ok()?
            .as_ref()
            .map(|p| p.before_query(sql))
    }

    fn stats_store_pending(&self, token: Option<Box<dyn Any + Send>>, sql: &str) {
        if let Some(token) = token {
            if let Ok(mut guard) = self.pending_stats.lock() {
                *guard = Some((token, sql.to_string()));
            }
        }
    }
}

impl AsyncConnection {
    // =========================================================================
    // Transaction Control
    // =========================================================================

    // -------------------------------------------------------------------
    // Raw transaction control (internal)
    // -------------------------------------------------------------------
    //
    // The `*_raw` methods below are `pub(crate)` and form the canonical
    // implementation of session-level transaction control. The RAII
    // guard at `crate::AsyncTransaction` and any internal helper that
    // genuinely needs `&self` (rather than the guard's `&mut self`)
    // delegate to these.
    //
    // The matching `pub` methods (`begin_transaction`, `commit`,
    // `rollback`) are thin `#[doc(hidden)] #[deprecated]` wrappers
    // retained only so any pre-existing downstream caller sees a
    // compiler warning rather than a hard break. They will be deleted
    // in a future release; the `_raw` methods stay.

    /// Issues `BEGIN TRANSACTION`. Crate-internal use only.
    pub(crate) async fn begin_transaction_raw(&self) -> Result<()> {
        self.execute_command("BEGIN TRANSACTION").await?;
        Ok(())
    }

    /// Issues `COMMIT`. Crate-internal use only.
    pub(crate) async fn commit_raw(&self) -> Result<()> {
        self.execute_command("COMMIT").await?;
        Ok(())
    }

    /// Issues `ROLLBACK`. Crate-internal use only.
    pub(crate) async fn rollback_raw(&self) -> Result<()> {
        self.execute_command("ROLLBACK").await?;
        Ok(())
    }

    /// Begins an explicit transaction (async).
    ///
    /// **Prefer [`transaction()`](Self::transaction)** — the RAII guard
    /// auto-rolls back on drop and cannot leak a half-open transaction
    /// across error paths. Hidden from generated rustdoc and
    /// deprecated; slated for removal in a future release.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects `BEGIN TRANSACTION`
    /// (e.g. a transaction is already open on this session).
    #[doc(hidden)]
    #[deprecated(
        note = "Use `AsyncConnection::transaction()` for an RAII guard. This method will be \
                removed in a future release."
    )]
    pub async fn begin_transaction(&self) -> Result<()> {
        self.begin_transaction_raw().await
    }

    /// Commits the current transaction (async).
    ///
    /// **Prefer [`AsyncTransaction::commit`](crate::AsyncTransaction::commit)**
    /// on the RAII guard returned by [`transaction()`](Self::transaction).
    /// Hidden from generated rustdoc and deprecated; slated for removal.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects `COMMIT`.
    #[doc(hidden)]
    #[deprecated(note = "Use `AsyncTransaction::commit()` on the RAII guard from \
                `AsyncConnection::transaction()`. This method will be removed in a future release.")]
    pub async fn commit(&self) -> Result<()> {
        self.commit_raw().await
    }

    /// Rolls back the current transaction (async).
    ///
    /// **Prefer [`AsyncTransaction::rollback`](crate::AsyncTransaction::rollback)**
    /// on the RAII guard returned by [`transaction()`](Self::transaction).
    /// Hidden from generated rustdoc and deprecated; slated for removal.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the server rejects `ROLLBACK`.
    #[doc(hidden)]
    #[deprecated(note = "Use `AsyncTransaction::rollback()` on the RAII guard from \
                `AsyncConnection::transaction()`. This method will be removed in a future release.")]
    pub async fn rollback(&self) -> Result<()> {
        self.rollback_raw().await
    }

    /// Starts a transaction with an async RAII guard (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the internal `BEGIN` issued by
    /// [`AsyncTransaction::new`](crate::AsyncTransaction) fails.
    pub async fn transaction(&mut self) -> Result<crate::AsyncTransaction<'_>> {
        crate::AsyncTransaction::new(self).await
    }
}
