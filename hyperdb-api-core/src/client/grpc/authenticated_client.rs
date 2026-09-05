// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Authenticated gRPC client with automatic DC JWT refresh.
//!
//! This module provides [`AuthenticatedGrpcClient`] which wraps the standard
//! gRPC client with automatic Salesforce Data Cloud token management.
//!
//! # Token Refresh Strategy
//!
//! The client handles DC JWT expiration in two complementary ways:
//!
//! 1. **Proactive refresh (maxAge + expiry threshold)**: Before each query,
//!    the client checks whether the DC JWT should be refreshed.  Refresh is
//!    triggered when *either*:
//!    - The DC JWT is within `threshold` seconds of its hard expiry
//!      (default: 300 s / 5 min), **or**
//!    - The DC JWT is older than `max_age` seconds (default: 900 s / 15 min).
//!
//!    The `maxAge` check ensures the underlying OAuth Access Token is
//!    revalidated regularly, catching Salesforce's server-side inactivity
//!    timeout before it causes a DC JWT exchange failure.
//!
//! 2. **Reactive refresh**: If a query fails with a gRPC `UNAUTHENTICATED`
//!    status, the client force-refreshes the DC JWT and retries once.
//!
//! This mirrors the proven ADAPTIVE-mode behavior from the C++
//! `GenieAuthorizationCallback` implementation.

use std::time::Duration;

use tonic::transport::{Channel, Endpoint};
use tracing::{debug, info, warn};

use std::collections::HashMap;

use crate::client::error::{Error, ErrorKind, Result};
use hyperdb_api_salesforce::{DataCloudToken, SharedTokenProvider};

use super::error::from_grpc_status;
use super::executor::GrpcQueryExecutor;
use super::params::{ParameterStyle, QueryParameters};
use super::proto::hyper_service::query_param::TransferMode;
use super::proto::{
    AttachedDatabase, CancelQueryParam, HyperServiceClient, OutputFormat, QueryParam,
};
use super::result::GrpcQueryResult;

/// Information about a database table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    /// The schema containing the table.
    pub schema: String,
    /// The table name (API name).
    pub name: String,
    /// The table type (TABLE, VIEW, or MATERIALIZED VIEW).
    pub table_type: String,
    /// The display name (if available, otherwise same as name).
    pub display_name: Option<String>,
}

impl TableInfo {
    /// Returns the fully qualified table name (schema.table).
    #[must_use]
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    /// Returns the display name if available, otherwise the API name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// Default DC JWT expiry threshold in seconds.
///
/// If the DC JWT expires within this many seconds, it is proactively refreshed
/// before query execution.  Matches the C++ `thresholdSeconds=300` value.
const DEFAULT_DC_JWT_EXPIRY_THRESHOLD_SECS: i64 = 300;

/// Default DC JWT max age in seconds.
///
/// If the DC JWT is older than this, it is proactively refreshed so that the
/// underlying OAuth Access Token is revalidated.  This catches Salesforce's
/// server-side inactivity timeout (org-dependent, commonly 15 min – 2 hours)
/// before it causes a DC JWT exchange failure.
///
/// Matches the C++ `maxAgeSeconds=900` value (15 minutes).
const DEFAULT_DC_JWT_MAX_AGE_SECS: i64 = 900;

/// Maximum number of retry attempts for authentication failures.
const MAX_AUTH_RETRIES: u32 = 1;

/// Authenticated gRPC client for Salesforce Data Cloud.
///
/// This client automatically manages DC JWT lifecycle, including proactive
/// refresh (based on both expiry threshold and max age) and reactive refresh
/// on gRPC `UNAUTHENTICATED` errors.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api_core::client::grpc::AuthenticatedGrpcClient;
/// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let login_url = "https://login.salesforce.com";
/// # let client_id = "client_id";
/// # let username = "user@example.com";
/// # let private_key = "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----";
/// # let dataspace: Option<String> = None;
/// // Create DC JWT provider
/// let auth_config = SalesforceAuthConfig::new(login_url, client_id)?
///     .auth_mode(AuthMode::private_key(username, private_key)?);
/// let token_provider = SharedTokenProvider::new(auth_config)?;
///
/// // Create authenticated client (obtains initial DC JWT)
/// let mut client = AuthenticatedGrpcClient::connect(token_provider, dataspace).await?;
///
/// // Execute queries — DC JWT refresh is handled automatically
/// let result = client.execute_query("SELECT * FROM my_table").await?;
/// # Ok(())
/// # }
/// ```
pub struct AuthenticatedGrpcClient {
    /// DC JWT provider for authentication
    token_provider: SharedTokenProvider,
    /// Optional dataspace for lakehouse name construction
    dataspace: Option<String>,
    /// gRPC channel (lazily connected)
    channel: Option<Channel>,
    /// Current DC JWT (cached for gRPC header construction)
    current_token: Option<DataCloudToken>,
    /// Proactive refresh: DC JWT expiry threshold in seconds.
    /// Refresh when the DC JWT has fewer than this many seconds remaining.
    dc_jwt_expiry_threshold_secs: i64,
    /// Proactive refresh: DC JWT max age in seconds.
    /// Refresh when the DC JWT is older than this to revalidate the
    /// OAuth Access Token against Salesforce's inactivity timeout.
    dc_jwt_max_age_secs: i64,
    /// Transfer mode for queries
    transfer_mode: TransferMode,
    /// Connect timeout
    connect_timeout: Duration,
    /// Request timeout
    request_timeout: Duration,
    /// Maximum size for decoding gRPC messages
    max_decoding_message_size: usize,
    /// Maximum size for encoding gRPC messages
    max_encoding_message_size: usize,
}

impl AuthenticatedGrpcClient {
    /// Creates a new authenticated client and connects to Data Cloud.
    ///
    /// This will:
    /// 1. Obtain an initial DC JWT from the provider
    /// 2. Extract the tenant URL from the DC JWT
    /// 3. Connect to the gRPC endpoint
    ///
    /// # Arguments
    ///
    /// * `token_provider` - Shared DC JWT provider for Salesforce authentication
    /// * `dataspace` - Optional dataspace name for multi-tenant environments
    ///
    /// # Errors
    ///
    /// Propagates any error from `Self::ensure_connected`: Salesforce
    /// auth failures (surfaced as [`ErrorKind::Authentication`]),
    /// invalid tenant URL (surfaced as [`ErrorKind::Config`]), or
    /// gRPC transport setup failures ([`ErrorKind::Connection`]).
    pub async fn connect(
        token_provider: SharedTokenProvider,
        dataspace: Option<String>,
    ) -> Result<Self> {
        use super::config::DEFAULT_MAX_MESSAGE_SIZE;

        let mut client = AuthenticatedGrpcClient {
            token_provider,
            dataspace,
            channel: None,
            current_token: None,
            dc_jwt_expiry_threshold_secs: DEFAULT_DC_JWT_EXPIRY_THRESHOLD_SECS,
            dc_jwt_max_age_secs: DEFAULT_DC_JWT_MAX_AGE_SECS,
            transfer_mode: TransferMode::Adaptive,
            connect_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(300),
            max_decoding_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            max_encoding_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        };

        // Obtain initial DC JWT and connect to the tenant gRPC endpoint
        client.ensure_connected().await?;

        Ok(client)
    }

    /// Sets the DC JWT expiry threshold for proactive refresh.
    ///
    /// If the DC JWT will expire within this many seconds, it will be
    /// proactively refreshed before executing a query.
    ///
    /// Default: 300 seconds (5 minutes).
    #[must_use]
    pub fn with_dc_jwt_expiry_threshold(mut self, secs: i64) -> Self {
        self.dc_jwt_expiry_threshold_secs = secs;
        self
    }

    /// Sets the DC JWT max age for proactive refresh.
    ///
    /// If the DC JWT is older than this many seconds, it will be proactively
    /// refreshed to revalidate the underlying OAuth Access Token.  This
    /// catches Salesforce's server-side inactivity timeout before it causes
    /// a DC JWT exchange failure.
    ///
    /// Default: 900 seconds (15 minutes).
    #[must_use]
    pub fn with_dc_jwt_max_age(mut self, secs: i64) -> Self {
        self.dc_jwt_max_age_secs = secs;
        self
    }

    /// Sets the transfer mode for queries.
    ///
    /// - `Sync`: Wait for complete results (best for small results)
    /// - `Async`: Stream results as they become available
    /// - `Adaptive`: Server chooses based on result size (default)
    #[must_use]
    pub fn with_transfer_mode(mut self, mode: TransferMode) -> Self {
        self.transfer_mode = mode;
        self
    }

    /// Sets the connection timeout.
    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Sets the request timeout.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Sets the maximum message size for both encoding and decoding.
    ///
    /// This is a convenience method that sets both limits to the same value.
    /// Default is 64 MB. You may need to increase this when using `TransferMode::Sync`
    /// with queries that return large result sets.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{AuthenticatedGrpcClient, TransferMode};
    /// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = SalesforceAuthConfig::new("https://login.salesforce.com", "client_id")?
    /// #     .auth_mode(AuthMode::password("user", "pass"));
    /// # let provider = SharedTokenProvider::new(config)?;
    /// # let dataspace: Option<String> = None;
    /// let client = AuthenticatedGrpcClient::connect(provider, dataspace).await?
    ///     .with_transfer_mode(TransferMode::Sync)
    ///     .with_max_message_size(256 * 1024 * 1024); // 256 MB
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn with_max_message_size(mut self, size: usize) -> Self {
        self.max_decoding_message_size = size;
        self.max_encoding_message_size = size;
        self
    }

    /// Sets the maximum size for decoding (receiving) gRPC messages.
    ///
    /// Default is 64 MB. This is particularly important when using `TransferMode::Sync`.
    #[must_use]
    pub fn with_max_decoding_message_size(mut self, size: usize) -> Self {
        self.max_decoding_message_size = size;
        self
    }

    /// Sets the maximum size for encoding (sending) gRPC messages.
    ///
    /// Default is 64 MB.
    #[must_use]
    pub fn with_max_encoding_message_size(mut self, size: usize) -> Self {
        self.max_encoding_message_size = size;
        self
    }

    /// Returns the current DC JWT if available.
    #[must_use]
    pub fn current_token(&self) -> Option<&DataCloudToken> {
        self.current_token.as_ref()
    }

    /// Returns the tenant URL from the current DC JWT.
    #[must_use]
    pub fn tenant_url(&self) -> Option<&str> {
        self.current_token
            .as_ref()
            .map(hyperdb_api_salesforce::DataCloudToken::tenant_url_str)
    }

    /// Executes a SQL query with automatic DC JWT refresh.
    ///
    /// If the DC JWT needs proactive refresh (near expiry or older than
    /// `max_age`), it is refreshed before executing the query.  If the
    /// query fails with gRPC `UNAUTHENTICATED`, the DC JWT is refreshed
    /// and the query retried once.
    ///
    /// # Errors
    ///
    /// Propagates any error from
    /// [`Self::execute_query_with_options`] — Salesforce auth failures,
    /// server-side SQL errors, or gRPC transport errors.
    pub async fn execute_query(&mut self, sql: &str) -> Result<GrpcQueryResult> {
        self.execute_query_with_options(sql, OutputFormat::ArrowIpc, self.transfer_mode)
            .await
    }

    /// Executes a query and returns raw Arrow IPC bytes.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::execute_query`].
    pub async fn execute_query_to_arrow(&mut self, sql: &str) -> Result<bytes::Bytes> {
        let result = self.execute_query(sql).await?;
        Ok(result.into_arrow_data())
    }

    /// Executes a parameterized SQL query with automatic token refresh.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{AuthenticatedGrpcClient, QueryParameters, ParameterStyle};
    /// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = SalesforceAuthConfig::new("https://login.salesforce.com", "client_id")?
    /// #     .auth_mode(AuthMode::password("user", "pass"));
    /// # let provider = SharedTokenProvider::new(config)?;
    /// # let mut client = AuthenticatedGrpcClient::connect(provider, None).await?;
    /// let params = QueryParameters::json_positional(&[&42i64])?;
    /// let result = client.execute_query_with_params(
    ///     "SELECT * FROM my_table WHERE id = $1",
    ///     params,
    ///     ParameterStyle::DollarNumbered,
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`Self::execute_query_with_params_and_options`].
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
            self.transfer_mode,
        )
        .await
    }

    /// Executes a parameterized query and returns raw Arrow IPC bytes.
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

    /// Executes a parameterized query with specific options and automatic token refresh.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Authentication`] if the DC JWT cannot be
    ///   refreshed through the underlying Salesforce token provider
    ///   (including after the auth-retry budget is exhausted).
    /// - Propagates any error from
    ///   [`GrpcClient::execute_query_with_params_and_options`](super::GrpcClient::execute_query_with_params_and_options) —
    ///   SQL / transport / protocol failures.
    pub async fn execute_query_with_params_and_options(
        &mut self,
        sql: &str,
        params: QueryParameters,
        style: ParameterStyle,
        output_format: OutputFormat,
        transfer_mode: TransferMode,
    ) -> Result<GrpcQueryResult> {
        // Ensure we have a valid token and connection
        self.ensure_token_valid().await?;

        // Try to execute the query
        let mut last_error = None;

        for attempt in 0..=MAX_AUTH_RETRIES {
            if attempt > 0 {
                info!(
                    "Retrying parameterized query after token refresh (attempt {})",
                    attempt + 1
                );
            }

            // Clone params for retry (if needed)
            let params_clone = params.clone();
            match self
                .execute_query_with_params_internal(
                    sql,
                    params_clone,
                    style,
                    output_format,
                    transfer_mode,
                )
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if Self::is_auth_error(&e) && attempt < MAX_AUTH_RETRIES {
                        warn!(
                            error = %e,
                            "Authentication error, refreshing token and retrying"
                        );

                        // Force token refresh and reconnect
                        if let Err(refresh_err) = self.force_refresh_and_reconnect().await {
                            warn!(error = %refresh_err, "Failed to refresh token");
                            return Err(e);
                        }

                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            Error::new(
                ErrorKind::Authentication,
                "Parameterized query failed after token refresh",
            )
        }))
    }

    /// Executes a query with specific options and automatic token refresh.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Authentication`] if every retry attempt
    ///   still surfaces an auth error after forcing a token refresh.
    /// - Propagates any other [`Error`] from the underlying gRPC
    ///   executor (SQL errors, transport failures).
    pub async fn execute_query_with_options(
        &mut self,
        sql: &str,
        output_format: OutputFormat,
        transfer_mode: TransferMode,
    ) -> Result<GrpcQueryResult> {
        // Ensure we have a valid token and connection
        self.ensure_token_valid().await?;

        // Try to execute the query
        let mut last_error = None;

        for attempt in 0..=MAX_AUTH_RETRIES {
            if attempt > 0 {
                info!(
                    "Retrying query after token refresh (attempt {})",
                    attempt + 1
                );
            }

            match self
                .execute_query_internal(sql, output_format, transfer_mode)
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if Self::is_auth_error(&e) && attempt < MAX_AUTH_RETRIES {
                        warn!(
                            error = %e,
                            "Authentication error, refreshing token and retrying"
                        );

                        // Force token refresh and reconnect
                        if let Err(refresh_err) = self.force_refresh_and_reconnect().await {
                            warn!(error = %refresh_err, "Failed to refresh token");
                            return Err(e);
                        }

                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            Error::new(
                ErrorKind::Authentication,
                "Query failed after token refresh",
            )
        }))
    }

    /// Forces a token refresh, even if the current token is still valid.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Authentication`] if
    ///   [`SharedTokenProvider::force_refresh`] fails.
    /// - Returns [`ErrorKind::Config`] or [`ErrorKind::Connection`]
    ///   if the fresh tenant URL is invalid or the gRPC channel
    ///   cannot be rebuilt.
    pub async fn refresh_token(&mut self) -> Result<()> {
        self.force_refresh_and_reconnect().await
    }

    /// Cancels a running query by ID with automatic token refresh.
    ///
    /// This is useful for long-running ASYNC or ADAPTIVE queries that may
    /// outlive the original token.
    ///
    /// # Arguments
    /// * `query_id` - The query ID returned from `GrpcQueryResult::query_id`
    ///
    /// # Returns
    /// Returns `Ok(())` on success, or an error if cancellation fails after retry.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::{AuthenticatedGrpcClient, OutputFormat, TransferMode};
    /// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = SalesforceAuthConfig::new("https://login.salesforce.com", "client_id")?
    /// #     .auth_mode(AuthMode::password("user", "pass"));
    /// # let provider = SharedTokenProvider::new(config)?;
    /// # let mut client = AuthenticatedGrpcClient::connect(provider, None).await?;
    /// # let sql = "SELECT * FROM my_table";
    /// // Execute a long-running ASYNC query
    /// let result = client.execute_query_with_options(
    ///     sql,
    ///     OutputFormat::ArrowIpc,
    ///     TransferMode::Async
    /// ).await?;
    ///
    /// // Store query_id for later cancellation
    /// if let Some(query_id) = result.query_id() {
    ///     // Later, even if token expired:
    ///     client.cancel_query(query_id).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Other`] if every retry attempt still
    ///   fails with an auth error after forcing a token refresh.
    /// - Propagates any error from
    ///   [`GrpcClient::cancel_query`](super::GrpcClient::cancel_query) (transport failure, `tonic::Status`).
    pub async fn cancel_query(&mut self, query_id: &str) -> Result<()> {
        // Ensure we have a valid token
        self.ensure_token_valid().await?;
        self.ensure_connected().await?;

        for attempt in 0..=MAX_AUTH_RETRIES {
            if attempt > 0 {
                info!(
                    "Retrying cancel after token refresh (attempt {})",
                    attempt + 1
                );
            }

            match self.cancel_query_internal(query_id).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if Self::is_auth_error(&e) && attempt < MAX_AUTH_RETRIES {
                        warn!(error = %e, "Auth error during cancel, refreshing token");
                        if let Err(refresh_err) = self.force_refresh_and_reconnect().await {
                            warn!(error = %refresh_err, "Failed to refresh token");
                            return Err(e);
                        }
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(Error::new(
            ErrorKind::Other,
            "Cancel failed after DC JWT refresh",
        ))
    }

    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "async fn retained for API symmetry; callers await regardless of whether the current body is synchronous"
    )]
    /// Closes the gRPC connection.
    ///
    /// # Errors
    ///
    /// Currently infallible — always returns `Ok(())`. The `Result`
    /// return type is preserved for API symmetry with the sync
    /// wrapper and for forward compatibility.
    pub async fn close(self) -> Result<()> {
        debug!("Closing authenticated gRPC connection");
        Ok(())
    }

    // ============================================================
    // Catalog Operations
    // ============================================================

    /// Returns a list of schema names from the database.
    ///
    /// This queries the `pg_catalog` to get all user-defined schemas,
    /// excluding system schemas like '`pg_catalog`', '`pg_temp`', and '`pg_toast`'.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::AuthenticatedGrpcClient;
    /// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = SalesforceAuthConfig::new("https://login.salesforce.com", "client_id")?
    /// #     .auth_mode(AuthMode::password("user", "pass"));
    /// # let provider = SharedTokenProvider::new(config)?;
    /// # let mut client = AuthenticatedGrpcClient::connect(provider, None).await?;
    /// let schemas = client.list_schemas().await?;
    /// for schema in schemas {
    ///     println!("Schema: {}", schema);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::execute_query`] plus any
    /// decoding error from `Self::extract_string_column` if the
    /// result payload is not the expected `TEXT`-column shape.
    pub async fn list_schemas(&mut self) -> Result<Vec<String>> {
        let query = r"
            SELECT nspname
            FROM pg_catalog.pg_namespace
            WHERE nspname NOT IN ('pg_catalog', 'pg_temp', 'pg_toast', 'information_schema')
            ORDER BY nspname
        ";

        let result = self.execute_query(query).await?;
        Self::extract_string_column(&result, 0)
    }

    /// Returns a list of table information from the database.
    ///
    /// This queries the `pg_catalog` to get all tables, views, and materialized views,
    /// excluding system schemas.
    ///
    /// # Returns
    ///
    /// A vector of [`TableInfo`] structs containing schema, table name, and table type.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::AuthenticatedGrpcClient;
    /// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = SalesforceAuthConfig::new("https://login.salesforce.com", "client_id")?
    /// #     .auth_mode(AuthMode::password("user", "pass"));
    /// # let provider = SharedTokenProvider::new(config)?;
    /// # let mut client = AuthenticatedGrpcClient::connect(provider, None).await?;
    /// let tables = client.list_tables().await?;
    /// for table in tables {
    ///     println!("{}.{} ({})", table.schema, table.name, table.table_type);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::list_tables_with_limit`].
    pub async fn list_tables(&mut self) -> Result<Vec<TableInfo>> {
        self.list_tables_with_limit(None).await
    }

    /// Returns a list of table information with an optional limit.
    ///
    /// # Arguments
    ///
    /// * `limit` - Optional maximum number of tables to return.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::execute_query`] plus any
    /// decoding failure from `Self::extract_table_info`.
    pub async fn list_tables_with_limit(&mut self, limit: Option<u32>) -> Result<Vec<TableInfo>> {
        let limit_clause = limit.map(|l| format!("LIMIT {l}")).unwrap_or_default();

        let query = format!(
            r"
            SELECT
                n.nspname AS table_schema,
                c.relname AS table_name,
                CASE c.relkind
                    WHEN 'r' THEN 'TABLE'
                    WHEN 'v' THEN 'VIEW'
                    WHEN 'm' THEN 'MATERIALIZED VIEW'
                    ELSE 'OTHER'
                END AS table_type
            FROM
                pg_catalog.pg_class c
                JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid
            WHERE
                c.relkind IN ('r', 'v', 'm')
                AND n.nspname NOT IN ('pg_catalog', 'pg_toast')
            ORDER BY
                n.nspname, c.relname
            {limit_clause}
            "
        );

        let result = self.execute_query(&query).await?;
        Self::extract_table_info(&result)
    }

    /// Returns a list of table names in a specific schema.
    ///
    /// # Arguments
    ///
    /// * `schema` - The schema name to query.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::AuthenticatedGrpcClient;
    /// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = SalesforceAuthConfig::new("https://login.salesforce.com", "client_id")?
    /// #     .auth_mode(AuthMode::password("user", "pass"));
    /// # let provider = SharedTokenProvider::new(config)?;
    /// # let mut client = AuthenticatedGrpcClient::connect(provider, None).await?;
    /// let tables = client.list_tables_in_schema("public").await?;
    /// for table in tables {
    ///     println!("Table: {}", table);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::list_schemas`].
    pub async fn list_tables_in_schema(&mut self, schema: &str) -> Result<Vec<String>> {
        let query = format!(
            r"
            SELECT c.relname AS table_name
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid
            WHERE c.relkind IN ('r', 'v', 'm')
              AND n.nspname = '{}'
            ORDER BY c.relname
            ",
            schema.replace('\'', "''")
        );

        let result = self.execute_query(&query).await?;
        Self::extract_string_column(&result, 0)
    }

    /// Checks if a table exists.
    ///
    /// # Arguments
    ///
    /// * `schema` - The schema name.
    /// * `table` - The table name.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::execute_query`] or from
    /// Arrow result decoding (unexpected schema on the probe query).
    pub async fn has_table(&mut self, schema: &str, table: &str) -> Result<bool> {
        let query = format!(
            r"
            SELECT 1
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid
            WHERE n.nspname = '{}' AND c.relname = '{}'
              AND c.relkind IN ('r', 'v', 'm')
            ",
            schema.replace('\'', "''"),
            table.replace('\'', "''")
        );

        let result = self.execute_query(&query).await?;
        Ok(!result.arrow_data().is_empty() && result.arrow_data().len() > 8)
    }

    /// Extracts a string column from Arrow IPC data.
    fn extract_string_column(result: &GrpcQueryResult, column_idx: usize) -> Result<Vec<String>> {
        use arrow::array::Array;
        use arrow::ipc::reader::StreamReader;
        use std::io::Cursor;

        let arrow_data = result.arrow_data();
        if arrow_data.is_empty() {
            return Ok(Vec::new());
        }

        let reader = StreamReader::try_new(Cursor::new(arrow_data), None).map_err(|e| {
            Error::new(
                ErrorKind::Protocol,
                format!("Failed to parse Arrow data: {e}"),
            )
        })?;

        let mut values = Vec::new();
        for batch_result in reader {
            let batch = batch_result.map_err(|e| {
                Error::new(
                    ErrorKind::Protocol,
                    format!("Failed to read Arrow batch: {e}"),
                )
            })?;

            if let Some(arr) = batch
                .column(column_idx)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
            {
                for i in 0..arr.len() {
                    if !arr.is_null(i) {
                        values.push(arr.value(i).to_string());
                    }
                }
            }
        }

        Ok(values)
    }

    /// Extracts table information from Arrow IPC data.
    fn extract_table_info(result: &GrpcQueryResult) -> Result<Vec<TableInfo>> {
        use arrow::array::Array;
        use arrow::ipc::reader::StreamReader;
        use std::io::Cursor;

        let arrow_data = result.arrow_data();
        if arrow_data.is_empty() {
            return Ok(Vec::new());
        }

        let reader = StreamReader::try_new(Cursor::new(arrow_data), None).map_err(|e| {
            Error::new(
                ErrorKind::Protocol,
                format!("Failed to parse Arrow data: {e}"),
            )
        })?;

        let mut tables = Vec::new();
        for batch_result in reader {
            let batch = batch_result.map_err(|e| {
                Error::new(
                    ErrorKind::Protocol,
                    format!("Failed to read Arrow batch: {e}"),
                )
            })?;

            let schema_col = batch
                .column(0)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>();
            let name_col = batch
                .column(1)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>();
            let type_col = batch
                .column(2)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>();

            if let (Some(schemas), Some(names), Some(types)) = (schema_col, name_col, type_col) {
                for i in 0..batch.num_rows() {
                    if !schemas.is_null(i) && !names.is_null(i) && !types.is_null(i) {
                        tables.push(TableInfo {
                            schema: schemas.value(i).to_string(),
                            name: names.value(i).to_string(),
                            table_type: types.value(i).to_string(),
                            display_name: None, // Will be populated by get_table_labels if needed
                        });
                    }
                }
            }
        }

        Ok(tables)
    }

    /// Returns a map of table names to their display labels for a given schema.
    ///
    /// Data Cloud stores table labels as JSON in `pg_description`: `{"displayName":"Label"}`.
    /// This method queries the metadata and parses the JSON to extract clean display names.
    ///
    /// # Arguments
    /// * `schema` - Schema name (e.g., "public")
    ///
    /// # Returns
    /// `HashMap` mapping table API names to display names
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::execute_query`]. Returns an
    /// Arrow IPC parse error (wrapped as [`ErrorKind::Other`]) when
    /// the result payload cannot be read as a `StreamReader`.
    pub async fn get_table_labels(
        &mut self,
        schema: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        let query = format!(
            r"SELECT c.relname as table_name,
                      COALESCE(d.description, c.relname) as label
               FROM pg_catalog.pg_class c
               LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = 0
               JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid
               WHERE n.nspname = '{schema}' AND c.relkind IN ('r', 'v', 'm')
               ORDER BY c.relname"
        );

        let result = self.execute_query(&query).await?;
        parse_label_pairs(&result.arrow_data())
    }

    /// Returns a map of column names to their display labels for a given table.
    ///
    /// Data Cloud stores field labels as JSON in `pg_description`: `{"displayName":"Label"}`.
    /// This method queries the metadata and parses the JSON to extract clean display names.
    ///
    /// # Arguments
    /// * `schema` - Schema name (e.g., "public")
    /// * `table` - Table name
    ///
    /// # Returns
    /// `HashMap` mapping column API names (e.g., "`elevation_ft__c`") to display names (e.g., "`elevation_ft`")
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::execute_query`]. Returns an
    /// Arrow IPC parse error (wrapped as [`ErrorKind::Other`]) when
    /// the result payload cannot be read as a `StreamReader`.
    pub async fn get_column_labels(
        &mut self,
        schema: &str,
        table: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        let query = format!(
            r"SELECT a.attname as column_name,
                      COALESCE(d.description, a.attname) as label
               FROM pg_catalog.pg_attribute a
               LEFT JOIN pg_catalog.pg_description d ON d.objoid = a.attrelid AND d.objsubid = a.attnum
               JOIN pg_catalog.pg_class c ON a.attrelid = c.oid
               JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid
               WHERE n.nspname = '{schema}' AND c.relname = '{table}' AND a.attnum > 0 AND NOT a.attisdropped
               ORDER BY a.attnum"
        );

        let result = self.execute_query(&query).await?;
        parse_label_pairs(&result.arrow_data())
    }

    /// Ensures we have a valid DC JWT, refreshing if necessary.
    ///
    /// Uses the same two-pronged check as the C++ `IsDCJWTExpiringSoon`:
    /// - **expiring**: DC JWT within `dc_jwt_expiry_threshold_secs` of hard expiry
    /// - **tooOld**: DC JWT older than `dc_jwt_max_age_secs`
    ///
    /// The `tooOld` check ensures the OAuth Access Token is revalidated
    /// regularly, catching Salesforce's server-side inactivity timeout.
    async fn ensure_token_valid(&mut self) -> Result<()> {
        let needs_refresh = match &self.current_token {
            Some(token) => {
                let needs = token
                    .needs_refresh(self.dc_jwt_expiry_threshold_secs, self.dc_jwt_max_age_secs);

                if needs {
                    let age_secs = token.age().num_seconds();
                    let remaining_secs = token.remaining_lifetime().num_seconds();
                    debug!(
                        age_secs,
                        remaining_secs,
                        max_age_secs = self.dc_jwt_max_age_secs,
                        threshold_secs = self.dc_jwt_expiry_threshold_secs,
                        "DC JWT needs proactive refresh (expiring or too old)"
                    );
                }

                needs
            }
            None => true,
        };

        if needs_refresh {
            self.force_refresh_and_reconnect().await?;
        }

        Ok(())
    }

    /// Ensures we have a gRPC connection with a valid DC JWT.
    async fn ensure_connected(&mut self) -> Result<()> {
        if self.channel.is_some() && self.current_token.is_some() {
            return Ok(());
        }

        let token = self.token_provider.get_token().await.map_err(|e| {
            Error::new(
                ErrorKind::Authentication,
                format!("Failed to get DC JWT: {e}"),
            )
        })?;

        self.connect_to_tenant(&token).await?;
        self.current_token = Some(token);

        Ok(())
    }

    /// Refreshes the DC JWT and reconnects.
    ///
    /// Uses [`SharedTokenProvider::refresh_token`] which reuses the cached
    /// OAuth Access Token if still valid, avoiding unnecessary OAuth Refresh
    /// Token rotation.
    async fn force_refresh_and_reconnect(&mut self) -> Result<()> {
        info!("Refreshing DC JWT");

        let token = self.token_provider.refresh_token().await.map_err(|e| {
            Error::new(
                ErrorKind::Authentication,
                format!("Failed to refresh DC JWT: {e}"),
            )
        })?;

        self.connect_to_tenant(&token).await?;
        self.current_token = Some(token);

        info!("DC JWT refreshed and reconnected successfully");
        Ok(())
    }

    /// Connects to the Data Cloud tenant using the DC JWT's tenant URL.
    async fn connect_to_tenant(&mut self, token: &DataCloudToken) -> Result<()> {
        let tenant_url = token.tenant_url();
        let hostname = tenant_url
            .host_str()
            .ok_or_else(|| Error::new(ErrorKind::Config, "No hostname in tenant URL"))?;

        let grpc_endpoint = format!("https://{hostname}:443");
        info!(endpoint = %grpc_endpoint, "Connecting to Data Cloud");

        let endpoint = Endpoint::from_shared(grpc_endpoint.clone())
            .map_err(|e| Error::new(ErrorKind::Config, format!("Invalid gRPC endpoint: {e}")))?;

        let endpoint = endpoint
            .connect_timeout(self.connect_timeout)
            .timeout(self.request_timeout);

        // Configure TLS
        let tls_config = tonic::transport::ClientTlsConfig::new().with_enabled_roots();
        let endpoint = endpoint
            .tls_config(tls_config)
            .map_err(|e| Error::new(ErrorKind::Config, format!("TLS configuration error: {e}")))?;

        let channel = endpoint.connect().await.map_err(|e| {
            Error::new(
                ErrorKind::Connection,
                format!("Failed to connect to {grpc_endpoint}: {e}"),
            )
        })?;

        self.channel = Some(channel);
        debug!("gRPC channel established");

        Ok(())
    }

    /// Executes a query using the current gRPC channel and DC JWT.
    async fn execute_query_internal(
        &mut self,
        sql: &str,
        output_format: OutputFormat,
        transfer_mode: TransferMode,
    ) -> Result<GrpcQueryResult> {
        self.execute_query_with_params_internal(
            sql,
            None,
            ParameterStyle::default(),
            output_format,
            transfer_mode,
        )
        .await
    }

    /// Executes a parameterized query using the current gRPC channel and DC JWT.
    async fn execute_query_with_params_internal(
        &mut self,
        sql: &str,
        params: impl Into<Option<QueryParameters>>,
        style: ParameterStyle,
        output_format: OutputFormat,
        transfer_mode: TransferMode,
    ) -> Result<GrpcQueryResult> {
        let channel = self
            .channel
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::Connection, "Not connected"))?;

        let token = self
            .current_token
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::Authentication, "No token available"))?;

        let params = params.into();
        debug!(
            sql = %sql,
            has_params = params.is_some(),
            format = ?output_format,
            mode = ?transfer_mode,
            "Executing query"
        );

        // Build the lakehouse name
        let lakehouse = token
            .lakehouse_name(self.dataspace.as_deref())
            .map_err(|e| {
                Error::new(
                    ErrorKind::Authentication,
                    format!("Failed to get lakehouse name: {e}"),
                )
            })?;

        // Build query parameter
        let query_param = QueryParam {
            query: sql.to_string(),
            databases: vec![AttachedDatabase {
                path: lakehouse,
                alias: String::new(),
            }],
            output_format: output_format.into(),
            settings: HashMap::new(),
            transfer_mode: transfer_mode.into(),
            param_style: i32::from(style),
            parameters: params.map(super::params::QueryParameters::into_proto),
            result_range: None,
            query_row_limit: None,
        };

        // Build headers
        let headers = vec![
            ("Authorization".to_string(), token.bearer_token()),
            ("audience".to_string(), token.tenant_url_str().to_string()),
        ];

        // Create executor with configured message size limits
        let client = HyperServiceClient::new(channel.clone())
            .max_decoding_message_size(self.max_decoding_message_size)
            .max_encoding_message_size(self.max_encoding_message_size);
        let mut executor = GrpcQueryExecutor::new(client, headers, transfer_mode);

        executor.execute(query_param).await?;

        // Collect results
        let mut final_result = GrpcQueryResult::default();

        loop {
            if let Some(mut partial_result) = executor.next_result().await? {
                while let Some(chunk) = partial_result.take_chunk() {
                    final_result.chunks.push_back(chunk);
                }
                if partial_result.query_id.is_some() {
                    final_result.query_id = partial_result.query_id;
                }
                if partial_result.schema.is_some() {
                    final_result.schema = partial_result.schema;
                }
                if partial_result.rows_affected.is_some() {
                    final_result.rows_affected = partial_result.rows_affected;
                }
                if partial_result.is_complete {
                    final_result.is_complete = true;
                    break;
                }
            } else {
                final_result.is_complete = true;
                break;
            }
        }

        Ok(final_result)
    }

    /// Internal cancel implementation without token refresh logic.
    async fn cancel_query_internal(&self, query_id: &str) -> Result<()> {
        let channel = self
            .channel
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::Connection, "Not connected"))?;

        let token = self
            .current_token
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::Authentication, "No token available"))?;

        debug!(query_id = %query_id, "Cancelling query");

        let param = CancelQueryParam {
            query_id: query_id.to_string(),
        };

        let mut request = tonic::Request::new(param);

        // Add headers
        if let Ok(value) = query_id.parse() {
            request.metadata_mut().insert("x-hyperdb-query-id", value);
        }
        request.metadata_mut().insert(
            "authorization",
            token
                .bearer_token()
                .parse()
                .map_err(|_| Error::new(ErrorKind::Authentication, "Invalid token format"))?,
        );
        request.metadata_mut().insert(
            "audience",
            token
                .tenant_url_str()
                .parse()
                .map_err(|_| Error::new(ErrorKind::Config, "Invalid tenant URL"))?,
        );

        let mut client = HyperServiceClient::new(channel.clone())
            .max_decoding_message_size(self.max_decoding_message_size)
            .max_encoding_message_size(self.max_encoding_message_size);
        client
            .cancel_query(request)
            .await
            .map_err(from_grpc_status)?;

        info!(query_id = %query_id, "Query cancelled successfully");
        Ok(())
    }

    /// Checks if an error is an authentication error that should trigger
    /// a DC JWT refresh and query retry.
    ///
    /// Only matches gRPC `UNAUTHENTICATED` (code 16) and HTTP 401 errors,
    /// which are the server-side signals that the DC JWT is no longer valid.
    /// Broader substring matches (e.g. "token", "expired") are intentionally
    /// avoided to prevent spurious retries on unrelated errors.
    fn is_auth_error(error: &Error) -> bool {
        if matches!(error.kind(), ErrorKind::Authentication) {
            return true;
        }

        let msg = error.to_string().to_lowercase();
        msg.contains("unauthenticated") || msg.contains("unauthorized") || msg.contains("401")
    }
}

impl std::fmt::Debug for AuthenticatedGrpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticatedGrpcClient")
            .field("dataspace", &self.dataspace)
            .field("has_channel", &self.channel.is_some())
            .field("has_dc_jwt", &self.current_token.is_some())
            .field(
                "dc_jwt_expiry_threshold_secs",
                &self.dc_jwt_expiry_threshold_secs,
            )
            .field("dc_jwt_max_age_secs", &self.dc_jwt_max_age_secs)
            .finish_non_exhaustive()
    }
}

/// Synchronous wrapper around [`AuthenticatedGrpcClient`].
///
/// Provides a blocking API by creating a Tokio runtime internally.
#[derive(Debug)]
pub struct AuthenticatedGrpcClientSync {
    inner: AuthenticatedGrpcClient,
    runtime: tokio::runtime::Runtime,
}

impl AuthenticatedGrpcClientSync {
    /// Creates a new authenticated client and connects (blocking).
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Other`] if a current-thread Tokio
    ///   runtime cannot be built.
    /// - Propagates any error from [`AuthenticatedGrpcClient::connect`].
    pub fn connect(token_provider: SharedTokenProvider, dataspace: Option<String>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::new(ErrorKind::Other, format!("Failed to create runtime: {e}")))?;

        let inner =
            runtime.block_on(AuthenticatedGrpcClient::connect(token_provider, dataspace))?;

        Ok(AuthenticatedGrpcClientSync { inner, runtime })
    }

    /// Executes a SQL query (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`AuthenticatedGrpcClient::execute_query`].
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

    /// Executes a parameterized SQL query (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`AuthenticatedGrpcClient::execute_query_with_params`].
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

    /// Forces a DC JWT refresh (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`AuthenticatedGrpcClient::refresh_token`].
    pub fn refresh_token(&mut self) -> Result<()> {
        self.runtime.block_on(self.inner.refresh_token())
    }

    /// Cancels a running query by ID (blocking).
    ///
    /// This is useful for long-running ASYNC or ADAPTIVE queries that may
    /// outlive the original DC JWT. DC JWT refresh is handled automatically.
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`AuthenticatedGrpcClient::cancel_query`].
    pub fn cancel_query(&mut self, query_id: &str) -> Result<()> {
        self.runtime.block_on(self.inner.cancel_query(query_id))
    }

    /// Returns the current DC JWT if available.
    pub fn current_token(&self) -> Option<&DataCloudToken> {
        self.inner.current_token()
    }

    /// Closes the gRPC connection (blocking).
    ///
    /// # Errors
    ///
    /// Currently infallible — always returns `Ok(())`. The `Result`
    /// return type is preserved for API symmetry.
    pub fn close(self) -> Result<()> {
        self.runtime.block_on(self.inner.close())
    }

    // ============================================================
    // Catalog Operations (Blocking)
    // ============================================================

    /// Returns a list of schema names from the database (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`AuthenticatedGrpcClient::list_schemas`].
    pub fn list_schemas(&mut self) -> Result<Vec<String>> {
        self.runtime.block_on(self.inner.list_schemas())
    }

    /// Returns a list of table information from the database (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`AuthenticatedGrpcClient::list_tables`].
    pub fn list_tables(&mut self) -> Result<Vec<TableInfo>> {
        self.runtime.block_on(self.inner.list_tables())
    }

    /// Returns a list of table information with an optional limit (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`AuthenticatedGrpcClient::list_tables_with_limit`].
    pub fn list_tables_with_limit(&mut self, limit: Option<u32>) -> Result<Vec<TableInfo>> {
        self.runtime
            .block_on(self.inner.list_tables_with_limit(limit))
    }

    /// Returns a list of table names in a specific schema (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`AuthenticatedGrpcClient::list_tables_in_schema`].
    pub fn list_tables_in_schema(&mut self, schema: &str) -> Result<Vec<String>> {
        self.runtime
            .block_on(self.inner.list_tables_in_schema(schema))
    }

    /// Checks if a table exists (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`AuthenticatedGrpcClient::has_table`].
    pub fn has_table(&mut self, schema: &str, table: &str) -> Result<bool> {
        self.runtime.block_on(self.inner.has_table(schema, table))
    }

    /// Returns a map of table names to their display labels for a given schema (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`AuthenticatedGrpcClient::get_table_labels`].
    pub fn get_table_labels(
        &mut self,
        schema: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        self.runtime.block_on(self.inner.get_table_labels(schema))
    }

    /// Returns a map of column names to their display labels for a given table (blocking).
    ///
    /// # Errors
    ///
    /// Same failure modes as
    /// [`AuthenticatedGrpcClient::get_column_labels`].
    pub fn get_column_labels(
        &mut self,
        schema: &str,
        table: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        self.runtime
            .block_on(self.inner.get_column_labels(schema, table))
    }
}

/// Parses an Arrow IPC stream of `(name, label)` text pairs into a map.
///
/// Shared by [`AuthenticatedGrpcClient::get_table_labels`] and
/// [`AuthenticatedGrpcClient::get_column_labels`], whose queries project the
/// same two `TEXT` columns and differ only in the catalog they read.
///
/// Data Cloud stores labels as JSON in `pg_description`
/// (`{"displayName":"Label"}`). A label that is not a JSON object, or whose
/// JSON lacks `displayName`, passes through verbatim — that fallback is
/// intentional, since plain-text descriptions are valid.
///
/// Rows where either column is NULL are skipped: the queries `COALESCE` the
/// description to the identifier, so a NULL means the catalog row itself is
/// unusable rather than that the label is absent.
///
/// # Errors
///
/// Returns [`crate::client::Error`] if the stream cannot be opened, a record
/// batch fails to decode, a batch projects fewer than two columns, or either
/// of the first two columns is not a `StringArray`.
///
/// Each of those used to be swallowed — the decode error and the type
/// mismatch by an `if let` chain, the short batch by an outright panic in
/// `RecordBatch::column`. Swallowing them produced a partial map that a caller
/// could not distinguish from "this table defines no labels", which is the
/// worst possible failure for metadata used to render UI.
fn parse_label_pairs(arrow_data: &[u8]) -> Result<std::collections::HashMap<String, String>> {
    use arrow::array::{Array, StringArray};

    let mut labels = std::collections::HashMap::new();

    let reader = arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(arrow_data), None)
        .map_err(|e| crate::client::Error::other(format!("Failed to parse Arrow data: {e}")))?;

    for batch_result in reader {
        let batch = batch_result.map_err(|e| {
            crate::client::Error::other(format!("Failed to decode Arrow record batch: {e}"))
        })?;

        // `RecordBatch::column` panics out of bounds, so check before indexing.
        if batch.num_columns() < 2 {
            return Err(crate::client::Error::other(format!(
                "label query must project 2 columns, got {}",
                batch.num_columns()
            )));
        }

        let downcast = |idx: usize| -> Result<&StringArray> {
            batch
                .column(idx)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| {
                    crate::client::Error::other(format!(
                        "label query column {idx} must be TEXT, got {:?}",
                        batch.column(idx).data_type()
                    ))
                })
        };
        let name_arr = downcast(0)?;
        let label_arr = downcast(1)?;

        for i in 0..batch.num_rows() {
            if name_arr.is_null(i) || label_arr.is_null(i) {
                continue;
            }
            let label_raw = label_arr.value(i);

            // Only attempt JSON when it looks like an object; a bare
            // description is the common case and need not go through serde.
            let label = if label_raw.starts_with('{') {
                serde_json::from_str::<serde_json::Value>(label_raw)
                    .ok()
                    .as_ref()
                    .and_then(|v| v.get("displayName"))
                    .and_then(serde_json::Value::as_str)
                    .map_or_else(|| label_raw.to_string(), std::string::ToString::to_string)
            } else {
                label_raw.to_string()
            };

            labels.insert(name_arr.value(i).to_string(), label);
        }
    }

    Ok(labels)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;

    use super::parse_label_pairs;

    /// Encodes one record batch as an Arrow IPC stream, as the server would.
    fn ipc_stream(batch: &RecordBatch) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = arrow::ipc::writer::StreamWriter::try_new(&mut buf, &batch.schema())
                .expect("create IPC writer");
            writer.write(batch).expect("write batch");
            writer.finish().expect("finish stream");
        }
        buf
    }

    fn two_text_batch(names: Vec<&str>, labels: Vec<&str>) -> RecordBatch {
        let schema = Schema::new(vec![
            Field::new("name", DataType::Utf8, true),
            Field::new("label", DataType::Utf8, true),
        ]);
        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(StringArray::from(names)) as ArrayRef,
                Arc::new(StringArray::from(labels)) as ArrayRef,
            ],
        )
        .expect("build batch")
    }

    #[test]
    fn extracts_display_name_from_json_and_passes_plain_text_through() {
        let batch = two_text_batch(
            vec!["accounts", "orders", "leads"],
            vec![
                r#"{"displayName":"Accounts"}"#,
                "Plain Description",
                // JSON object without displayName falls back to the raw text.
                r#"{"other":"x"}"#,
            ],
        );
        let labels = parse_label_pairs(&ipc_stream(&batch)).expect("valid stream parses");

        assert_eq!(labels.get("accounts").map(String::as_str), Some("Accounts"));
        assert_eq!(
            labels.get("orders").map(String::as_str),
            Some("Plain Description")
        );
        assert_eq!(
            labels.get("leads").map(String::as_str),
            Some(r#"{"other":"x"}"#),
            "a JSON object without displayName must pass through verbatim"
        );
    }

    #[test]
    fn truncated_stream_is_an_error_not_a_partial_map() {
        // The regression this guards: a decode failure used to be swallowed by
        // `if let Ok(batch)`, yielding an empty map that a caller could not
        // tell apart from "this table defines no labels".
        let full = ipc_stream(&two_text_batch(vec!["accounts"], vec!["Accounts"]));
        let truncated = &full[..full.len() / 2];

        let err = parse_label_pairs(truncated)
            .expect_err("a truncated Arrow stream must not silently yield a partial map");
        let msg = format!("{err}");
        assert!(
            msg.contains("Arrow"),
            "error should name the Arrow failure, got: {msg}"
        );
    }

    #[test]
    fn non_text_column_is_an_error() {
        // Previously the `downcast_ref` tuple silently skipped the whole batch.
        let schema = Schema::new(vec![
            Field::new("name", DataType::Utf8, true),
            Field::new("label", DataType::Int32, true),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(StringArray::from(vec!["accounts"])) as ArrayRef,
                Arc::new(Int32Array::from(vec![1])) as ArrayRef,
            ],
        )
        .expect("build batch");

        let err = parse_label_pairs(&ipc_stream(&batch))
            .expect_err("a non-TEXT label column must be reported, not skipped");
        assert!(
            format!("{err}").contains("must be TEXT"),
            "error should explain the type mismatch, got: {err}"
        );
    }

    #[test]
    fn single_column_batch_errors_instead_of_panicking() {
        // `RecordBatch::column(1)` panics out of bounds; the guard turns that
        // into a diagnosable error.
        let schema = Schema::new(vec![Field::new("name", DataType::Utf8, true)]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![Arc::new(StringArray::from(vec!["accounts"])) as ArrayRef],
        )
        .expect("build batch");

        let err = parse_label_pairs(&ipc_stream(&batch))
            .expect_err("a one-column batch must error rather than panic");
        assert!(
            format!("{err}").contains("must project 2 columns"),
            "error should name the column-count problem, got: {err}"
        );
    }

    #[test]
    fn null_rows_are_skipped_without_failing() {
        let schema = Schema::new(vec![
            Field::new("name", DataType::Utf8, true),
            Field::new("label", DataType::Utf8, true),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(StringArray::from(vec![Some("accounts"), None])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("Accounts"), Some("Orphan")])) as ArrayRef,
            ],
        )
        .expect("build batch");

        let labels = parse_label_pairs(&ipc_stream(&batch)).expect("nulls are not an error");
        assert_eq!(labels.len(), 1, "the NULL-name row should be skipped");
        assert_eq!(labels.get("accounts").map(String::as_str), Some("Accounts"));
    }
}
