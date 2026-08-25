// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! gRPC client for Hyper database.
//!
//! This module provides the [`GrpcClient`] struct for executing queries against
//! Hyper servers via gRPC.

use std::sync::Arc;

use tonic::transport::{Channel, Endpoint};
use tracing::{debug, info, warn};

use crate::client::error::{Error, ErrorKind, Result};

use super::config::GrpcConfig;
use super::error::from_grpc_status;
use super::executor::{GrpcChunkStream, GrpcQueryExecutor};
use super::params::{ParameterStyle, QueryParameters};
use super::proto::hyper_service::query_param::TransferMode;
use super::proto::{
    AttachedDatabase, CancelQueryParam, HyperServiceClient, OutputFormat, QueryParam,
};
use super::result::GrpcQueryResult;

/// Async gRPC client for Hyper database.
///
/// `GrpcClient` provides query-only access to Hyper databases via gRPC.
/// Results are returned in Apache Arrow IPC format.
///
/// gRPC transport is always available - no feature flags required.
///
/// # Limitations
///
/// The gRPC interface is **read-only**:
/// - Only SELECT queries are supported
/// - No INSERT, UPDATE, DELETE, or DDL operations
/// - No COPY protocol for bulk data insertion
///
/// For write operations, use the standard TCP [`Client`](crate::client::Client).
///
/// # Example
///
/// ```no_run
/// use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let config = GrpcConfig::new("http://localhost:7484")
///         .database("my_database.hyper");
///
///     let mut client = GrpcClient::connect(config).await?;
///
///     // Execute a query
///     let result = client.execute_query("SELECT * FROM users").await?;
///     let arrow_data = result.arrow_data();
///
///     // Process arrow_data with arrow crate...
///
///     client.close().await?;
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct GrpcClient {
    /// The underlying gRPC channel
    channel: Channel,
    /// Client configuration
    config: GrpcConfig,
}

impl GrpcClient {
    /// Connects to a Hyper server via gRPC.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let config = GrpcConfig::new("http://localhost:7484")
    ///     .database("test.hyper");
    ///
    /// let client = GrpcClient::connect(config).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Config`] if `config.endpoint` is not a
    ///   well-formed URI, or if TLS configuration fails.
    /// - Returns [`ErrorKind::Connection`] if the gRPC transport
    ///   cannot establish a channel to the endpoint.
    pub async fn connect(config: GrpcConfig) -> Result<Self> {
        info!(endpoint = %config.endpoint, "Connecting to Hyper via gRPC");

        let endpoint = Endpoint::from_shared(config.endpoint.clone())
            .map_err(|e| Error::new(ErrorKind::Config, format!("Invalid gRPC endpoint: {e}")))?;

        // Configure timeouts
        let endpoint = endpoint
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout);

        // Configure TLS if needed
        let endpoint = if config.use_tls {
            // Use system root certificates for TLS validation
            let tls_config = tonic::transport::ClientTlsConfig::new().with_enabled_roots();

            endpoint.tls_config(tls_config).map_err(|e| {
                Error::new(ErrorKind::Config, format!("TLS configuration error: {e}"))
            })?
        } else {
            endpoint
        };

        // Connect
        let channel = endpoint.connect().await.map_err(|e| {
            debug!("gRPC connection error details: {:?}", e);
            Error::new(
                ErrorKind::Connection,
                format!("Failed to connect to gRPC endpoint: {e} (details: {e:?})"),
            )
        })?;

        debug!("gRPC channel established");

        Ok(GrpcClient { channel, config })
    }

    /// Returns the underlying gRPC channel.
    ///
    /// This can be used for advanced use cases like channel cloning or
    /// direct stub access.
    #[must_use]
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Returns the client configuration.
    pub fn config(&self) -> &GrpcConfig {
        &self.config
    }

    /// Executes a SQL query and returns the result.
    ///
    /// Results are returned in Apache Arrow IPC format. Use the `arrow_data()`
    /// method on the result to get the raw Arrow bytes.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = GrpcConfig::new("http://localhost:7484");
    /// # let mut client = GrpcClient::connect(config).await?;
    /// let result = client.execute_query("SELECT * FROM users LIMIT 10").await?;
    /// let arrow_bytes = result.arrow_data();
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The query syntax is invalid
    /// - The referenced tables/columns don't exist
    /// - A non-SELECT query is executed (gRPC is read-only)
    /// - The connection is lost
    pub async fn execute_query(&mut self, sql: &str) -> Result<GrpcQueryResult> {
        self.execute_query_with_options(sql, OutputFormat::ArrowIpc, self.config.transfer_mode)
            .await
    }

    /// Executes a query and returns raw Arrow IPC bytes.
    ///
    /// This is a convenience method that extracts the Arrow data from the result.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = GrpcConfig::new("http://localhost:7484");
    /// # let mut client = GrpcClient::connect(config).await?;
    /// let arrow_bytes = client.execute_query_to_arrow("SELECT * FROM users").await?;
    /// // Parse with arrow crate...
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::execute_query`] (invalid SQL,
    /// missing tables/columns, non-SELECT mutation attempts, or
    /// connection loss).
    pub async fn execute_query_to_arrow(&mut self, sql: &str) -> Result<bytes::Bytes> {
        let result = self.execute_query(sql).await?;
        Ok(result.into_arrow_data())
    }

    /// Executes a parameterized SQL query.
    ///
    /// This provides SQL injection prevention and type safety by separating
    /// the query from its parameters.
    ///
    /// # Arguments
    ///
    /// * `sql` - SQL query with parameter placeholders
    /// * `params` - Query parameters (JSON or Arrow encoded)
    /// * `style` - Parameter style used in the query
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig, QueryParameters, ParameterStyle};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = GrpcConfig::new("http://localhost:7484");
    /// # let mut client = GrpcClient::connect(config).await?;
    /// // Dollar-numbered parameters (mixed types use from_json_value)
    /// let params = QueryParameters::from_json_value(&serde_json::json!([42, "Alice"]))?;
    /// let result = client.execute_query_with_params(
    ///     "SELECT * FROM users WHERE id = $1 AND name = $2",
    ///     params,
    ///     ParameterStyle::DollarNumbered,
    /// ).await?;
    ///
    /// // Named parameters
    /// let params = QueryParameters::json_named()
    ///     .add("min_age", &18)?
    ///     .build();
    /// let result = client.execute_query_with_params(
    ///     "SELECT * FROM users WHERE age >= :min_age",
    ///     params,
    ///     ParameterStyle::Named,
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::execute_query`], plus any
    /// parameter-related error reported by the server (unknown
    /// placeholder, type coercion failure, shape mismatch between the
    /// SQL placeholders and the supplied parameter set).
    pub async fn execute_query_with_params(
        &mut self,
        sql: &str,
        params: QueryParameters,
        style: ParameterStyle,
    ) -> Result<GrpcQueryResult> {
        self.execute_query_with_params_and_options(
            sql,
            params,
            style,
            OutputFormat::ArrowIpc,
            self.config.transfer_mode,
        )
        .await
    }

    /// Executes a parameterized query and returns raw Arrow IPC bytes.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig, QueryParameters, ParameterStyle};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = GrpcConfig::new("http://localhost:7484");
    /// # let mut client = GrpcClient::connect(config).await?;
    /// let params = QueryParameters::json_positional(&[&42i64])?;
    /// let arrow_bytes = client.execute_query_with_params_to_arrow(
    ///     "SELECT * FROM users WHERE id = $1",
    ///     params,
    ///     ParameterStyle::DollarNumbered,
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::execute_query_with_params`].
    pub async fn execute_query_with_params_to_arrow(
        &mut self,
        sql: &str,
        params: QueryParameters,
        style: ParameterStyle,
    ) -> Result<bytes::Bytes> {
        let result = self.execute_query_with_params(sql, params, style).await?;
        Ok(result.into_arrow_data())
    }

    /// Executes a parameterized query with specific options.
    ///
    /// This allows full control over the output format and transfer mode.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Protocol`] if the server returns no
    ///   result chunks and does not signal completion.
    /// - Propagates any error from the underlying
    ///   `GrpcQueryExecutor` — auth failure, transport error, or
    ///   server-side SQL error surfaced as [`tonic::Status`].
    pub async fn execute_query_with_params_and_options(
        &mut self,
        sql: &str,
        params: QueryParameters,
        style: ParameterStyle,
        output_format: OutputFormat,
        transfer_mode: TransferMode,
    ) -> Result<GrpcQueryResult> {
        debug!(
            sql = %sql,
            param_style = ?style,
            format = ?output_format,
            mode = ?transfer_mode,
            "Executing parameterized query"
        );

        // Build the query parameter
        let query_param = QueryParam {
            query: sql.to_string(),
            databases: self.build_attached_databases(),
            output_format: output_format.into(),
            settings: self.config.settings.clone(),
            transfer_mode: transfer_mode.into(),
            param_style: i32::from(style),
            parameters: Some(params.into_proto()),
            result_range: None,
            query_row_limit: None,
        };

        // Build headers for authentication and routing
        let headers = self.build_headers();

        // Create executor with configured message size limits
        let client = HyperServiceClient::new(self.channel.clone())
            .max_decoding_message_size(self.config.max_decoding_message_size)
            .max_encoding_message_size(self.config.max_encoding_message_size);
        let mut executor = GrpcQueryExecutor::new(client, headers, transfer_mode);

        // Execute the query
        executor.execute(query_param).await?;

        // Collect all result chunks
        let mut final_result = GrpcQueryResult::default();

        loop {
            if let Some(mut partial_result) = executor.next_result().await? {
                // Merge chunks
                while let Some(chunk) = partial_result.take_chunk() {
                    final_result.chunks.push_back(chunk);
                }
                // Copy metadata from last result
                if partial_result.query_id.is_some() {
                    final_result.query_id = partial_result.query_id;
                }
                if partial_result.schema.is_some() {
                    final_result.schema = partial_result.schema;
                }
                if partial_result.rows_affected.is_some() {
                    final_result.rows_affected = partial_result.rows_affected;
                }
                // Check if complete
                if partial_result.is_complete {
                    final_result.is_complete = true;
                    break;
                }
            } else {
                // No more results
                final_result.is_complete = true;
                break;
            }
        }

        if final_result.chunks.is_empty() && !final_result.is_complete {
            return Err(Error::new(ErrorKind::Protocol, "No result from query"));
        }

        Ok(final_result)
    }

    /// Executes a query with specific options.
    ///
    /// This allows control over the output format and transfer mode.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Protocol`] if the server returns no
    ///   result chunks and does not signal completion.
    /// - Propagates any error from the underlying
    ///   `GrpcQueryExecutor` — auth failure, transport error, or
    ///   server-side SQL error surfaced as [`tonic::Status`].
    pub async fn execute_query_with_options(
        &mut self,
        sql: &str,
        output_format: OutputFormat,
        transfer_mode: TransferMode,
    ) -> Result<GrpcQueryResult> {
        debug!(sql = %sql, format = ?output_format, mode = ?transfer_mode, "Executing query");

        // Build the query parameter
        let query_param = QueryParam {
            query: sql.to_string(),
            databases: self.build_attached_databases(),
            output_format: output_format.into(),
            settings: self.config.settings.clone(),
            transfer_mode: transfer_mode.into(),
            param_style: 0, // Default
            parameters: None,
            result_range: None,
            query_row_limit: None,
        };

        // Build headers for authentication and routing
        let headers = self.build_headers();

        // Create executor with configured message size limits
        let client = HyperServiceClient::new(self.channel.clone())
            .max_decoding_message_size(self.config.max_decoding_message_size)
            .max_encoding_message_size(self.config.max_encoding_message_size);
        let mut executor = GrpcQueryExecutor::new(client, headers, transfer_mode);

        // Execute the query
        executor.execute(query_param).await?;

        // Collect all result chunks
        let mut final_result = GrpcQueryResult::default();

        loop {
            if let Some(mut partial_result) = executor.next_result().await? {
                // Merge chunks
                while let Some(chunk) = partial_result.take_chunk() {
                    final_result.chunks.push_back(chunk);
                }
                // Copy metadata from last result
                if partial_result.query_id.is_some() {
                    final_result.query_id = partial_result.query_id;
                }
                if partial_result.schema.is_some() {
                    final_result.schema = partial_result.schema;
                }
                if partial_result.rows_affected.is_some() {
                    final_result.rows_affected = partial_result.rows_affected;
                }
                // Check if complete
                if partial_result.is_complete {
                    final_result.is_complete = true;
                    break;
                }
            } else {
                // No more results
                final_result.is_complete = true;
                break;
            }
        }

        if final_result.chunks.is_empty() && !final_result.is_complete {
            return Err(Error::new(ErrorKind::Protocol, "No result from query"));
        }

        Ok(final_result)
    }

    /// Executes a query and returns a streaming chunk producer.
    ///
    /// Unlike [`execute_query`](Self::execute_query), which drains every
    /// result chunk into a single [`GrpcQueryResult`] before returning, this
    /// method yields chunks lazily: each call to
    /// [`GrpcChunkStream::next_chunk`] pulls just enough from the HTTP/2
    /// stream to produce one Arrow IPC byte chunk. For very large result
    /// sets (hundreds of MB to GB) this keeps client memory bounded by a
    /// single gRPC message (capped at the tonic
    /// `max_decoding_message_size`, default 64 MB) rather than growing to
    /// the full result size.
    ///
    /// Pair this with
    /// [`hyperdb_api::ArrowRowset::from_stream`][arrow_rowset_from_stream] to
    /// decode batches incrementally and keep peak memory constant regardless
    /// of total row count.
    ///
    /// [arrow_rowset_from_stream]: https://docs.rs/hyperdb-api/latest/hyperdb_api/struct.ArrowRowset.html#method.from_stream
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`Self::execute_query_stream_with_options`] — invalid SQL,
    /// auth failure, transport error, etc.
    pub async fn execute_query_stream(&mut self, sql: &str) -> Result<GrpcChunkStream> {
        self.execute_query_stream_with_options(
            sql,
            OutputFormat::ArrowIpc,
            self.config.transfer_mode,
        )
        .await
    }

    /// Streaming variant of [`execute_query_with_options`](Self::execute_query_with_options).
    ///
    /// # Errors
    ///
    /// Propagates any error from the initial
    /// `GrpcQueryExecutor::execute` call — server-side SQL error,
    /// auth failure, or transport-level gRPC error.
    pub async fn execute_query_stream_with_options(
        &mut self,
        sql: &str,
        output_format: OutputFormat,
        transfer_mode: TransferMode,
    ) -> Result<GrpcChunkStream> {
        debug!(sql = %sql, format = ?output_format, mode = ?transfer_mode, "Executing streaming query");

        let query_param = QueryParam {
            query: sql.to_string(),
            databases: self.build_attached_databases(),
            output_format: output_format.into(),
            settings: self.config.settings.clone(),
            transfer_mode: transfer_mode.into(),
            param_style: 0,
            parameters: None,
            result_range: None,
            query_row_limit: None,
        };

        let headers = self.build_headers();

        let client = HyperServiceClient::new(self.channel.clone())
            .max_decoding_message_size(self.config.max_decoding_message_size)
            .max_encoding_message_size(self.config.max_encoding_message_size);
        let mut executor = GrpcQueryExecutor::new(client, headers, transfer_mode);

        executor.execute(query_param).await?;

        Ok(GrpcChunkStream::new(executor))
    }

    /// Cancels an in-flight gRPC query by its `query_id`.
    ///
    /// This is the gRPC analogue of the PG wire `CancelRequest` packet: it
    /// tells the server to stop executing a previously-started query. Unlike
    /// PG wire (where the cancel travels on a *fresh* connection), gRPC
    /// cancels travel as a regular RPC multiplexed over the existing HTTP/2
    /// channel — that's why this call shares `self.channel` with normal
    /// query traffic.
    ///
    /// # When do you have a `query_id`?
    ///
    /// The server assigns a `query_id` for queries started in
    /// [`TransferMode::Async`](super::proto::hyper_service::query_param::TransferMode)
    /// (long-running queries that the client polls). Grab it from
    /// [`GrpcQueryResult::query_id`](super::result::GrpcQueryResult::query_id)
    /// after `execute_query_with_options(..., TransferMode::Async)` returns.
    /// SYNC-mode queries typically complete before the client needs a
    /// cancel — for those, just drop the in-flight future.
    ///
    /// # Query-id lifecycle
    ///
    /// Query ids are stable for the lifetime of a query and are
    /// server-assigned — a given id is never silently re-used for a
    /// different query (Hyper generates them as UUID-like opaque tokens,
    /// not sequential counters). The only race a caller needs to
    /// consider is between obtaining the id and calling `cancel_query`:
    ///
    /// - If the query is still running, the cancel lands and the server
    ///   aborts it.
    /// - If the query has already completed normally between "obtain id"
    ///   and "cancel", the server sees a cancel for an unknown /
    ///   completed query and handles it gracefully (the exact shape
    ///   depends on server build — see the tests in
    ///   `hyperdb-api/tests/grpc_cancel_tests.rs` for details). Either way
    ///   the channel stays healthy.
    ///
    /// There is no scenario where a stale id causes a cancel to target
    /// the *wrong* query, because ids are not reassigned.
    ///
    /// # Errors
    ///
    /// Propagates transport-level errors. A successful cancel returns
    /// `Ok(())` even if the query had already completed on the server;
    /// cancellation is best-effort by design.
    ///
    /// # Relation to the [`Cancellable`](crate::client::cancel::Cancellable) trait
    ///
    /// This is the **fallible user-facing cancel API**: it returns a
    /// `Result<()>` so explicit callers can observe transport-level
    /// failures and react accordingly.
    ///
    /// It is *not* an implementation of the
    /// [`Cancellable`](crate::client::cancel::Cancellable) trait — and cannot
    /// be, because `Cancellable::cancel(&self)` takes no arguments while
    /// gRPC cancels need a per-query `query_id`. A `GrpcClient` can have
    /// many concurrent queries in flight; there is no single "the"
    /// query on it the way there is on a PG wire connection. A future
    /// gRPC streaming result type (when one is introduced) would carry
    /// its `query_id` in a dedicated handle like
    /// `GrpcCancelHandle { client, query_id }`, and *that* handle
    /// would `impl Cancellable` by wrapping this method and swallowing
    /// errors — same shape as
    /// [`impl Cancellable for Client`](crate::client::cancel::Cancellable).
    /// See the [`Cancellable`](crate::client::cancel::Cancellable) trait docs
    /// for the full wrapper pattern.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig, OutputFormat, TransferMode};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = GrpcConfig::new("http://localhost:7484");
    /// # let mut client = GrpcClient::connect(config).await?;
    /// let result = client
    ///     .execute_query_with_options(
    ///         "SELECT * FROM very_large_table",
    ///         OutputFormat::ArrowIpc,
    ///         TransferMode::Async,
    ///     )
    ///     .await?;
    ///
    /// if let Some(query_id) = result.query_id() {
    ///     // Some time later, decide to abort:
    ///     client.cancel_query(query_id).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn cancel_query(&mut self, query_id: &str) -> Result<()> {
        debug!(query_id = %query_id, "Cancelling gRPC query");

        let param = CancelQueryParam {
            query_id: query_id.to_string(),
        };
        let mut request = tonic::Request::new(param);

        // Apply the client's standard headers (database routing, custom
        // headers). Matches what the query path does so that any server-side
        // routing based on headers lands the cancel on the same backend as
        // the query it's trying to cancel.
        //
        // Header parse failures are logged at `warn!` and then skipped —
        // the cancel goes out without that particular header rather than
        // failing the whole operation. A missing custom header is strictly
        // better than a cancel we never send. The warn! is the only
        // operational signal that routing-critical headers (e.g. the
        // database selector) were dropped, so don't silence it.
        for (key, value) in self.build_headers() {
            match (
                key.parse::<tonic::metadata::MetadataKey<_>>(),
                value.parse(),
            ) {
                (Ok(k), Ok(v)) => {
                    request.metadata_mut().insert(k, v);
                }
                (key_res, value_res) => {
                    warn!(
                        target: "hyperdb_api_core::client",
                        query_id = %query_id,
                        header_key = %key,
                        key_parse_ok = key_res.is_ok(),
                        value_parse_ok = value_res.is_ok(),
                        "cancel: header parse failed, dropping header from cancel request",
                    );
                }
            }
        }
        // Also set the canonical x-hyperdb-query-id metadata — some server
        // deployments route cancels based on this header rather than the
        // payload body.
        match query_id.parse() {
            Ok(value) => {
                request.metadata_mut().insert("x-hyperdb-query-id", value);
            }
            Err(e) => {
                warn!(
                    target: "hyperdb_api_core::client",
                    query_id = %query_id,
                    error = %e,
                    "cancel: x-hyperdb-query-id header parse failed; \
                     cancel routing may fall back to payload-based lookup",
                );
            }
        }

        let mut client = HyperServiceClient::new(self.channel.clone())
            .max_decoding_message_size(self.config.max_decoding_message_size)
            .max_encoding_message_size(self.config.max_encoding_message_size);
        client
            .cancel_query(request)
            .await
            .map_err(from_grpc_status)?;

        info!(query_id = %query_id, "gRPC query cancelled");
        Ok(())
    }

    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "async fn retained for API symmetry; callers await regardless of whether the current body is synchronous"
    )]
    /// Closes the gRPC connection.
    ///
    /// This is a no-op as tonic channels are reference-counted and will be
    /// closed when the last reference is dropped.
    ///
    /// # Errors
    ///
    /// Currently infallible — always returns `Ok(())`. The `Result`
    /// return type is preserved for API symmetry with
    /// [`GrpcClientSync::close`] and for forward compatibility if
    /// future tonic channels expose a fallible shutdown.
    pub async fn close(self) -> Result<()> {
        debug!("Closing gRPC connection");
        // Channel is dropped automatically
        Ok(())
    }

    /// Builds the attached databases list from configuration.
    fn build_attached_databases(&self) -> Vec<AttachedDatabase> {
        if let Some(db_path) = &self.config.database {
            debug!(db_path = %db_path, "Attaching database for query");
            // Check if it's a JSON array (multiple databases)
            if db_path.starts_with('[') {
                // Parse JSON - for now just use as single database
                // TODO: Implement proper JSON parsing for multiple databases
                vec![AttachedDatabase {
                    path: db_path.clone(),
                    alias: String::new(), // Empty alias means use default
                }]
            } else {
                vec![AttachedDatabase {
                    path: db_path.clone(),
                    alias: String::new(), // Empty alias means use default
                }]
            }
        } else {
            debug!("No database configured on gRPC client — query will run without attachment");
            vec![]
        }
    }

    /// Builds headers for gRPC requests.
    fn build_headers(&self) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = self
            .config
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Add database header if configured
        if let Some(ref db) = self.config.database {
            headers.push(("x-hyper-database".to_string(), db.clone()));
        }

        headers
    }
}

/// Synchronous wrapper around [`GrpcClient`].
///
/// This provides a blocking API by creating a Tokio runtime internally.
/// For better performance in async contexts, use [`GrpcClient`] directly.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api_core::client::grpc::{GrpcClientSync, GrpcConfig};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let config = GrpcConfig::new("http://localhost:7484")
///     .database("test.hyper");
///
/// let mut client = GrpcClientSync::connect(config)?;
/// let result = client.execute_query("SELECT * FROM users")?;
/// let arrow_bytes = result.arrow_data();
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct GrpcClientSync {
    /// The async client
    inner: GrpcClient,
    /// Tokio runtime for blocking operations.
    ///
    /// Wrapped in `Arc` so streaming chunk producers
    /// ([`GrpcChunkStreamSync`]) can share the same runtime without having to
    /// borrow the client or create their own.
    runtime: Arc<tokio::runtime::Runtime>,
}

impl GrpcClientSync {
    /// Connects to a Hyper server via gRPC (blocking).
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Other`] if a current-thread Tokio
    ///   runtime cannot be built.
    /// - Propagates any error from [`GrpcClient::connect`] (invalid
    ///   endpoint, TLS configuration failure, or transport setup
    ///   failure).
    pub fn connect(config: GrpcConfig) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                Error::new(
                    ErrorKind::Other,
                    format!("Failed to create Tokio runtime: {e}"),
                )
            })?;

        let inner = runtime.block_on(GrpcClient::connect(config))?;

        Ok(GrpcClientSync {
            inner,
            runtime: Arc::new(runtime),
        })
    }

    /// Executes a SQL query (blocking).
    ///
    /// # Errors
    ///
    /// Blocking wrapper around [`GrpcClient::execute_query`]; see that
    /// method for the concrete failure modes.
    pub fn execute_query(&mut self, sql: &str) -> Result<GrpcQueryResult> {
        self.runtime.block_on(self.inner.execute_query(sql))
    }

    /// Executes a query and returns Arrow IPC bytes (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::execute_query`].
    pub fn execute_query_to_arrow(&mut self, sql: &str) -> Result<bytes::Bytes> {
        self.runtime
            .block_on(self.inner.execute_query_to_arrow(sql))
    }

    /// Executes a query and returns a blocking streaming chunk producer.
    ///
    /// See [`GrpcClient::execute_query_stream`] for the streaming semantics
    /// and memory behavior. The returned [`GrpcChunkStreamSync`] lets you
    /// pull chunks one at a time without buffering the entire result.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`GrpcClient::execute_query_stream`].
    pub fn execute_query_stream(&mut self, sql: &str) -> Result<GrpcChunkStreamSync> {
        let inner = self
            .runtime
            .block_on(self.inner.execute_query_stream(sql))?;
        Ok(GrpcChunkStreamSync {
            inner,
            runtime: Arc::clone(&self.runtime),
        })
    }

    /// Executes a parameterized SQL query (blocking).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{GrpcClientSync, GrpcConfig, QueryParameters, ParameterStyle};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = GrpcConfig::new("http://localhost:7484");
    /// # let mut client = GrpcClientSync::connect(config)?;
    /// let params = QueryParameters::json_positional(&[&42i64])?;
    /// let result = client.execute_query_with_params(
    ///     "SELECT * FROM users WHERE id = $1",
    ///     params,
    ///     ParameterStyle::DollarNumbered,
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Blocking wrapper around
    /// [`GrpcClient::execute_query_with_params`]; see that method for
    /// the concrete failure modes.
    pub fn execute_query_with_params(
        &mut self,
        sql: &str,
        params: QueryParameters,
        style: ParameterStyle,
    ) -> Result<GrpcQueryResult> {
        self.runtime
            .block_on(self.inner.execute_query_with_params(sql, params, style))
    }

    /// Executes a parameterized query and returns Arrow IPC bytes (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::execute_query_with_params`].
    pub fn execute_query_with_params_to_arrow(
        &mut self,
        sql: &str,
        params: QueryParameters,
        style: ParameterStyle,
    ) -> Result<bytes::Bytes> {
        self.runtime.block_on(
            self.inner
                .execute_query_with_params_to_arrow(sql, params, style),
        )
    }

    /// Cancels an in-flight gRPC query by its `query_id` (blocking).
    ///
    /// Blocking wrapper around
    /// [`GrpcClient::cancel_query`]. See that method's documentation for
    /// when a `query_id` is available (ASYNC-mode queries), best-effort
    /// cancel semantics, and the full "Relation to the `Cancellable`
    /// trait" discussion.
    ///
    /// # Fallible by design
    ///
    /// The `Result<()>` return is intentional and mirrors the async
    /// `GrpcClient::cancel_query`. Explicit callers get to observe
    /// transport-level failures (network errors, channel closed, auth
    /// expired) so they can record metrics, retry, or surface "cancel
    /// failed" UX. This is *not* an `impl Cancellable for GrpcClientSync`
    /// — it cannot be, because `Cancellable::cancel(&self)` takes no
    /// arguments and has no way to pass the `query_id`.  See the
    /// [`Cancellable`](crate::client::cancel::Cancellable) trait docs for the
    /// infallible-wrapper pattern used by `Drop`-path consumers.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{GrpcClientSync, GrpcConfig};
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = GrpcConfig::new("http://localhost:7484");
    /// # let mut client = GrpcClientSync::connect(config)?;
    /// # let query_id = "some-query-id";
    /// client.cancel_query(query_id)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Same failure modes as [`GrpcClient::cancel_query`] —
    /// transport-level errors bubble up; a cancel for an
    /// already-completed query returns `Ok(())` by design.
    pub fn cancel_query(&mut self, query_id: &str) -> Result<()> {
        self.runtime.block_on(self.inner.cancel_query(query_id))
    }

    /// Returns the client configuration.
    pub fn config(&self) -> &GrpcConfig {
        self.inner.config()
    }

    /// Closes the connection (blocking).
    ///
    /// # Errors
    ///
    /// Currently infallible — always returns `Ok(())`. The `Result`
    /// return type is preserved for API symmetry with async callers.
    pub fn close(self) -> Result<()> {
        self.runtime.block_on(self.inner.close())
    }
}

/// Blocking wrapper around [`GrpcChunkStream`].
///
/// Returned by [`GrpcClientSync::execute_query_stream`] and the
/// `AuthenticatedGrpcClientSync` equivalent. Yields Arrow IPC byte chunks
/// one at a time, blocking on the shared Tokio runtime as needed.
///
/// Pair with
/// [`hyperdb_api::ArrowRowset::from_stream`][arrow_rowset_from_stream] to
/// decode Arrow record batches incrementally with constant client memory.
///
/// [arrow_rowset_from_stream]: https://docs.rs/hyperdb-api/latest/hyperdb_api/struct.ArrowRowset.html#method.from_stream
#[derive(Debug)]
pub struct GrpcChunkStreamSync {
    inner: GrpcChunkStream,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl GrpcChunkStreamSync {
    /// Returns the next Arrow IPC byte chunk, or `None` when the stream is
    /// complete.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`GrpcChunkStream::next_chunk`] —
    /// transport errors and server-side query failures surface as
    /// [`Error`].
    pub fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>> {
        self.runtime.block_on(self.inner.next_chunk())
    }

    /// Returns the schema reported by the server, if one has been received yet.
    pub fn schema(&self) -> Option<&super::proto::QueryResultSchema> {
        self.inner.schema()
    }

    /// Returns the server-assigned query ID, if one has been received.
    pub fn query_id(&self) -> Option<&str> {
        self.inner.query_id()
    }

    /// Returns the affected row count for DML queries, if reported.
    pub fn rows_affected(&self) -> Option<u64> {
        self.inner.rows_affected()
    }
}
