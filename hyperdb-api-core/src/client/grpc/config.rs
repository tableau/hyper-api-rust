// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! gRPC connection configuration.
//!
//! This module provides the [`GrpcConfig`] type for configuring gRPC connections
//! to Hyper servers.

use std::collections::HashMap;
use std::time::Duration;

use super::proto::hyper_service::query_param::TransferMode as ProtoTransferMode;

/// How the server delivers query results.
///
/// This is the crate's own enum rather than the generated protobuf one, so
/// regenerating the proto does not change the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferMode {
    /// All results arrive in the `ExecuteQuery` response (simple; subject to
    /// the server's 100 s timeout).
    Sync,
    /// The `ExecuteQuery` response carries only a header; results are fetched
    /// with `GetQueryResult`.
    Async,
    /// The first chunk arrives inline and the rest is streamed through
    /// `GetQueryResult` (the default, and recommended).
    #[default]
    Adaptive,
}

impl From<TransferMode> for ProtoTransferMode {
    fn from(mode: TransferMode) -> Self {
        match mode {
            TransferMode::Sync => ProtoTransferMode::Sync,
            TransferMode::Async => ProtoTransferMode::Async,
            TransferMode::Adaptive => ProtoTransferMode::Adaptive,
        }
    }
}

impl From<TransferMode> for i32 {
    fn from(mode: TransferMode) -> Self {
        ProtoTransferMode::from(mode).into()
    }
}

/// Default maximum message size for gRPC requests/responses (64 MB).
///
/// This is larger than tonic's default (4 MB) to accommodate large query results
/// when using `TransferMode::Sync`. For `Adaptive` or `Async` modes, results are
/// streamed in smaller chunks, so this limit is less likely to be hit.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024; // 64 MB

/// Configuration for gRPC connections to Hyper servers.
///
/// # Example
///
/// ```
/// use hyperdb_api_core::client::grpc::GrpcConfig;
/// use std::time::Duration;
///
/// let config = GrpcConfig::new("http://localhost:7484")
///     .database("my_database.hyper")
///     .connect_timeout(Duration::from_secs(30))
///     .request_timeout(Duration::from_secs(60));
/// ```
///
/// # Message Size Limits
///
/// By default, the client uses a 64 MB message size limit. This is important when
/// using `TransferMode::Sync`, which returns all results in a single response.
/// For `TransferMode::Adaptive` (the default) or `TransferMode::Async`, results
/// are streamed in chunks, making large message sizes less critical.
///
/// ```
/// use hyperdb_api_core::client::grpc::{GrpcConfig, TransferMode};
///
/// // For very large SYNC results, increase the limit:
/// let config = GrpcConfig::new("http://localhost:7484")
///     .transfer_mode(TransferMode::Sync)
///     .max_message_size(256 * 1024 * 1024); // 256 MB
/// ```
#[derive(Debug, Clone)]
#[must_use = "GrpcConfig uses a consuming builder pattern - each method takes ownership and returns a new instance. You must use the returned value or your configuration changes will be lost"]
pub struct GrpcConfig {
    /// The endpoint URL (e.g., "<http://localhost:7484>" or "<https://hyper.example.com:443>")
    pub(crate) endpoint: String,

    /// Database path(s) to attach. Can be a single path or JSON array for multiple databases.
    pub(crate) database: Option<String>,

    /// Connection timeout
    pub(crate) connect_timeout: Duration,

    /// Request timeout (per-request)
    pub(crate) request_timeout: Duration,

    /// Transfer mode for query results
    pub(crate) transfer_mode: TransferMode,

    /// Use TLS (https) - automatically detected from endpoint URL
    pub(crate) use_tls: bool,

    /// Additional headers to send with requests (for authentication, routing, etc.)
    pub(crate) headers: HashMap<String, String>,

    /// Connection settings (passed as query parameters to Hyper)
    pub(crate) settings: HashMap<String, String>,

    /// Maximum size for decoding (receiving) gRPC messages in bytes.
    /// Default is 64 MB. This is particularly important for `TransferMode::Sync`.
    pub(crate) max_decoding_message_size: usize,

    /// Maximum size for encoding (sending) gRPC messages in bytes.
    /// Default is 64 MB.
    pub(crate) max_encoding_message_size: usize,
}

impl GrpcConfig {
    /// Creates a new gRPC configuration with the given endpoint.
    ///
    /// The endpoint should be a URL like `http://localhost:7484` or
    /// `https://hyper-service.example.com:443`.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::GrpcConfig;
    ///
    /// let config = GrpcConfig::new("http://localhost:7484");
    /// ```
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into();
        let use_tls = endpoint.starts_with("https://");

        GrpcConfig {
            endpoint,
            database: None,
            connect_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(100), // Match Hyper's default 100s timeout
            transfer_mode: TransferMode::Adaptive,     // Best default for most workloads
            use_tls,
            headers: HashMap::new(),
            settings: HashMap::new(),
            max_decoding_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            max_encoding_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        }
    }

    // Builder Methods (All now automatically protected by the struct-level #[must_use])

    /// Sets the database path to attach.
    ///
    /// For a single database, provide the path directly. For multiple databases,
    /// provide a JSON array like `[{"path": "db1.hyper", "alias": "db1"}, ...]`.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::GrpcConfig;
    ///
    /// let config = GrpcConfig::new("http://localhost:7484")
    ///     .database("my_database.hyper");
    /// ```
    pub fn database(mut self, database: impl Into<String>) -> Self {
        self.database = Some(database.into());
        self
    }

    /// Sets the connection timeout.
    ///
    /// This is the maximum time to wait for the initial connection to be established.
    /// Default is 30 seconds.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Sets the request timeout.
    ///
    /// This is the maximum time to wait for a single request to complete.
    /// Default is 100 seconds (matching Hyper's server-side timeout for SYNC mode).
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Sets the transfer mode for query results.
    ///
    /// - `TransferMode::Sync` - All results in `ExecuteQuery` response (simple, 100s timeout)
    /// - `TransferMode::Async` - Header only, fetch results via `GetQueryResult`
    /// - `TransferMode::Adaptive` - First chunk inline, rest via `GetQueryResult` (default)
    ///
    /// `Adaptive` is recommended for most workloads as it provides the best balance
    /// of latency and reliability.
    pub fn transfer_mode(mut self, mode: TransferMode) -> Self {
        self.transfer_mode = mode;
        self
    }

    /// Sets the maximum message size for both encoding (sending) and decoding (receiving).
    ///
    /// This is a convenience method that sets both `max_decoding_message_size` and
    /// `max_encoding_message_size` to the same value.
    ///
    /// Default is 64 MB. You may need to increase this when using `TransferMode::Sync`
    /// with queries that return large result sets.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::{GrpcConfig, TransferMode};
    ///
    /// // Allow up to 256 MB messages for large SYNC queries
    /// let config = GrpcConfig::new("http://localhost:7484")
    ///     .transfer_mode(TransferMode::Sync)
    ///     .max_message_size(256 * 1024 * 1024);
    /// ```
    pub fn max_message_size(mut self, size: usize) -> Self {
        self.max_decoding_message_size = size;
        self.max_encoding_message_size = size;
        self
    }

    /// Sets the maximum size for decoding (receiving) gRPC messages.
    ///
    /// Default is 64 MB. This is particularly important when using `TransferMode::Sync`,
    /// which returns all query results in a single response message. If your queries
    /// return more data than this limit, you will receive a "message too large" error.
    ///
    /// For `TransferMode::Adaptive` (default) or `TransferMode::Async`, results are
    /// streamed in smaller chunks, making this limit less critical.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::GrpcConfig;
    ///
    /// let config = GrpcConfig::new("http://localhost:7484")
    ///     .max_decoding_message_size(128 * 1024 * 1024); // 128 MB
    /// ```
    pub fn max_decoding_message_size(mut self, size: usize) -> Self {
        self.max_decoding_message_size = size;
        self
    }

    /// Sets the maximum size for encoding (sending) gRPC messages.
    ///
    /// Default is 64 MB. This affects the size of requests sent to the server,
    /// which is typically small for queries but may be larger for parameterized
    /// queries with large parameter payloads.
    pub fn max_encoding_message_size(mut self, size: usize) -> Self {
        self.max_encoding_message_size = size;
        self
    }

    /// Adds a header to send with all requests.
    ///
    /// This is useful for authentication tokens, routing hints, or other metadata.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::GrpcConfig;
    ///
    /// let config = GrpcConfig::new("https://hyper.example.com")
    ///     .header("authorization", "Bearer my-token")
    ///     .header("x-tenant-id", "tenant-123");
    /// ```
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// Adds multiple headers at once.
    pub fn headers(mut self, headers: impl IntoIterator<Item = (String, String)>) -> Self {
        self.headers.extend(headers);
        self
    }

    /// Adds a connection setting.
    ///
    /// These settings are passed to Hyper as query parameters.
    /// See Hyper documentation for available settings.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::GrpcConfig;
    ///
    /// let config = GrpcConfig::new("http://localhost:7484")
    ///     .setting("log_level", "debug");
    /// ```
    pub fn setting(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.settings.insert(key.into(), value.into());
        self
    }

    /// Returns the endpoint URL.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the database path, if set.
    #[must_use]
    pub fn database_path(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// Returns whether TLS is enabled.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.use_tls
    }

    /// Returns the maximum decoding message size.
    #[must_use]
    pub fn get_max_decoding_message_size(&self) -> usize {
        self.max_decoding_message_size
    }

    /// Returns the maximum encoding message size.
    #[must_use]
    pub fn get_max_encoding_message_size(&self) -> usize {
        self.max_encoding_message_size
    }

    /// Configures authentication headers from a DC JWT.
    ///
    /// Sets the `Authorization` and `audience` gRPC headers from the DC JWT.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_core::client::grpc::GrpcConfig;
    /// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, DataCloudTokenProvider};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let config = SalesforceAuthConfig::new("https://login.salesforce.com", "client_id")?
    /// #     .auth_mode(AuthMode::password("user", "pass"));
    /// // Get a DC JWT
    /// let mut provider = DataCloudTokenProvider::new(config)?;
    /// let dc_jwt = provider.get_token().await?;
    ///
    /// // Configure gRPC client with the DC JWT
    /// let grpc_config = GrpcConfig::new("https://hyper.data.salesforce.com")
    ///     .with_data_cloud_token(&dc_jwt);
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "salesforce-auth")]
    pub fn with_data_cloud_token(self, token: &hyperdb_api_salesforce::DataCloudToken) -> Self {
        self.header("Authorization", token.bearer_token())
            .header("audience", token.tenant_url_str())
    }

    /// Configures authentication headers with bearer token and audience.
    ///
    /// This is a lower-level method for manually setting authentication headers.
    ///
    /// # Arguments
    ///
    /// * `bearer_token` - The full Authorization header value (e.g., "Bearer abc123...")
    /// * `audience` - The tenant URL or audience value
    pub fn with_bearer_auth(
        self,
        bearer_token: impl Into<String>,
        audience: impl Into<String>,
    ) -> Self {
        self.header("Authorization", bearer_token)
            .header("audience", audience)
    }
}

impl Default for GrpcConfig {
    fn default() -> Self {
        GrpcConfig::new("http://localhost:7484")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_builder() {
        let config = GrpcConfig::new("http://localhost:7484")
            .database("test.hyper")
            .connect_timeout(Duration::from_secs(10))
            .request_timeout(Duration::from_secs(30))
            .header("x-custom", "value")
            .setting("log_level", "debug");

        assert_eq!(config.endpoint, "http://localhost:7484");
        assert_eq!(config.database, Some("test.hyper".to_string()));
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert!(!config.use_tls);
        assert_eq!(config.headers.get("x-custom"), Some(&"value".to_string()));
        assert_eq!(config.settings.get("log_level"), Some(&"debug".to_string()));
    }

    #[expect(
        clippy::similar_names,
        reason = "paired bindings (request/response, reader/writer, etc.) are more readable with symmetric names than artificially distinct ones"
    )]
    #[test]
    fn test_tls_detection() {
        let http_config = GrpcConfig::new("http://localhost:7484");
        assert!(!http_config.use_tls);

        let https_config = GrpcConfig::new("https://hyper.example.com:443");
        assert!(https_config.use_tls);
    }

    #[test]
    fn test_default_values() {
        let config = GrpcConfig::default();
        assert_eq!(config.endpoint, "http://localhost:7484");
        assert_eq!(config.connect_timeout, Duration::from_secs(30));
        assert_eq!(config.request_timeout, Duration::from_secs(100));
        assert!(matches!(config.transfer_mode, TransferMode::Adaptive));
        assert_eq!(config.max_decoding_message_size, DEFAULT_MAX_MESSAGE_SIZE);
        assert_eq!(config.max_encoding_message_size, DEFAULT_MAX_MESSAGE_SIZE);
    }

    #[test]
    fn test_message_size_configuration() {
        // Test max_message_size sets both
        let config = GrpcConfig::new("http://localhost:7484").max_message_size(128 * 1024 * 1024);
        assert_eq!(config.max_decoding_message_size, 128 * 1024 * 1024);
        assert_eq!(config.max_encoding_message_size, 128 * 1024 * 1024);

        // Test individual setters
        let config = GrpcConfig::new("http://localhost:7484")
            .max_decoding_message_size(256 * 1024 * 1024)
            .max_encoding_message_size(32 * 1024 * 1024);
        assert_eq!(config.max_decoding_message_size, 256 * 1024 * 1024);
        assert_eq!(config.max_encoding_message_size, 32 * 1024 * 1024);

        // Test getters
        assert_eq!(config.get_max_decoding_message_size(), 256 * 1024 * 1024);
        assert_eq!(config.get_max_encoding_message_size(), 32 * 1024 * 1024);
    }

    #[test]
    fn test_sync_mode_with_large_message_size() {
        // Common pattern: SYNC mode with increased message size for large results
        let config = GrpcConfig::new("http://localhost:7484")
            .transfer_mode(TransferMode::Sync)
            .max_message_size(256 * 1024 * 1024);

        assert!(matches!(config.transfer_mode, TransferMode::Sync));
        assert_eq!(config.max_decoding_message_size, 256 * 1024 * 1024);
    }
}
