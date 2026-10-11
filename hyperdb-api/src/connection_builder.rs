// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::connection::{Connection, CreateMode};
use crate::error::{Error, Result};
use crate::transport::{Transport, TransportType, detect_transport_type};
use hyperdb_api_core::client::Config;
use hyperdb_api_core::client::tls::{TlsConfig, TlsMode};

/// A builder for creating database connections.
///
/// This provides a flexible way to configure a connection with various options
/// like authentication, database creation mode, and transport settings.
///
/// # Transport Auto-Detection
///
/// The transport is automatically detected from the endpoint URL:
/// - `https://` or `http://` → gRPC (read-only)
/// - `tab.domain://...` or an absolute socket path → Unix domain socket (Unix only)
/// - `tab.pipe://...` or a `\\host\pipe\name` path → named pipe (Windows only)
/// - otherwise `host:port` → TCP (e.g., `localhost:7483`)
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{ConnectionBuilder, CreateMode, Result};
///
/// fn main() -> Result<()> {
///     // TCP connection
///     let conn = ConnectionBuilder::new("localhost:7483")
///         .database("example.hyper")
///         .create_mode(CreateMode::CreateIfNotExists)
///         .user("myuser")
///         .password("mypassword")
///         .build()?;
///     Ok(())
/// }
/// ```
///
/// ```no_run
/// # use hyperdb_api::{ConnectionBuilder, Result};
/// # fn example() -> Result<()> {
/// // gRPC connection (auto-detected from URL)
/// let conn = ConnectionBuilder::new("https://hyper-server.example.com:443")
///     .database("example.hyper")
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ConnectionBuilder {
    endpoint: String,
    database: Option<PathBuf>,
    create_mode: CreateMode,
    user: Option<String>,
    password: Option<String>,
    login_timeout: Option<Duration>,
    /// Per-query timeout, sent to the server as the `query_timeout` setting.
    query_timeout: Option<Duration>,
    /// Application name sent to the server during connection startup.
    application_name: Option<String>,
    /// Transfer mode for gRPC connections (ignored for TCP)
    transfer_mode: Option<crate::grpc::TransferMode>,
    /// TLS settings for TCP connections.
    tls: TlsConfig,
}

impl Default for ConnectionBuilder {
    fn default() -> Self {
        Self::new("localhost:7483")
    }
}

/// Renders the `SET` statement that applies `timeout` as the session's
/// `query_timeout` (whole milliseconds, rounded up).
///
/// `hyperd` ignores `query_timeout` as a startup parameter, so it has to be
/// set on the established session.
pub(crate) fn query_timeout_statement(timeout: Duration) -> Result<String> {
    if timeout.is_zero() {
        return Err(Error::config("query_timeout must be greater than zero"));
    }
    let millis = timeout.as_nanos().div_ceil(1_000_000);
    Ok(format!("SET query_timeout = '{millis}ms'"))
}

impl ConnectionBuilder {
    /// Creates a new builder for the given endpoint.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The server endpoint; see [`ConnectionBuilder`] for the accepted forms.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            database: None,
            create_mode: CreateMode::default(),
            user: Some("tableau_internal_user".to_string()),
            password: None,
            login_timeout: None,
            query_timeout: None,
            application_name: None,
            transfer_mode: None, // Use default (Adaptive)
            tls: TlsConfig::default(),
        }
    }

    /// Sets the database path.
    #[must_use]
    pub fn database(mut self, path: impl AsRef<Path>) -> Self {
        self.database = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the database creation mode.
    ///
    /// Default is `CreateMode::DoNotCreate`.
    #[must_use]
    pub fn create_mode(mut self, mode: CreateMode) -> Self {
        self.create_mode = mode;
        self
    }

    /// Sets the username for authentication.
    ///
    /// Default is "`tableau_internal_user`".
    #[must_use]
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Sets the password for authentication.
    #[must_use]
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Sets the login timeout (TCP connections only).
    #[must_use]
    pub fn login_timeout(mut self, timeout: Duration) -> Self {
        self.login_timeout = Some(timeout);
        self
    }

    /// Sets the query timeout.
    ///
    /// Applied to the session as `hyperd`'s `query_timeout` setting right after
    /// connecting, so the server cancels any statement on this connection that
    /// runs longer than `timeout` and reports an error. Sub-millisecond
    /// precision is rounded up to a whole millisecond.
    ///
    /// Applies to TCP, Unix-socket and named-pipe connections. `hyperd`
    /// clamps the value to its own `query_timeout_max` ceiling. A gRPC
    /// connection has no equivalent, so [`build`](Self::build) fails with
    /// [`Error::FeatureNotSupported`] rather than ignoring the setting.
    ///
    /// # Errors
    ///
    /// [`build`](Self::build) returns an error if `timeout` is zero.
    #[must_use]
    pub fn query_timeout(mut self, timeout: Duration) -> Self {
        self.query_timeout = Some(timeout);
        self
    }

    /// Sets the application name sent to the server.
    ///
    /// This appears in server logs and can be used for monitoring. It applies
    /// to TCP connections only.
    #[must_use]
    pub fn application_name(mut self, name: impl Into<String>) -> Self {
        self.application_name = Some(name.into());
        self
    }

    /// Convenience method to set user and password at once.
    #[must_use]
    pub fn auth(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self.password = Some(password.into());
        self
    }

    /// Convenience method to create a new database.
    #[must_use]
    pub fn create_new_database(mut self, database_path: impl AsRef<Path>) -> Self {
        self.database = Some(database_path.as_ref().to_path_buf());
        self.create_mode = CreateMode::Create;
        self
    }

    /// Convenience method to create database if it doesn't exist.
    #[must_use]
    pub fn create_or_open_database(mut self, database_path: impl AsRef<Path>) -> Self {
        self.database = Some(database_path.as_ref().to_path_buf());
        self.create_mode = CreateMode::CreateIfNotExists;
        self
    }

    /// Convenience method to open an existing database.
    #[must_use]
    pub fn open_database(mut self, database_path: impl AsRef<Path>) -> Self {
        self.database = Some(database_path.as_ref().to_path_buf());
        self.create_mode = CreateMode::DoNotCreate;
        self
    }

    /// Sets the TLS configuration for a TCP connection.
    ///
    /// The default is [`TlsMode::Disable`]. A bare [`TlsMode`] converts into a
    /// [`TlsConfig`]; use [`TlsConfig`] to add a root certificate, a client
    /// certificate for mutual TLS, or a server-name override. The modes follow
    /// libpq's `sslmode`:
    ///
    /// | Mode | Encrypted | Server certificate checked |
    /// |------|-----------|----------------------------|
    /// | [`Disable`](TlsMode::Disable) | never | — |
    /// | [`Prefer`](TlsMode::Prefer) | if the server offers TLS, else plaintext | chain, only if a root certificate is set |
    /// | [`Require`](TlsMode::Require) | always | chain, only if a root certificate is set |
    /// | [`VerifyCa`](TlsMode::VerifyCa) | always | chain, against the root certificate |
    /// | [`VerifyFull`](TlsMode::VerifyFull) | always | chain and host name |
    ///
    /// Over a Unix domain socket or a named pipe, [`TlsMode::Prefer`]
    /// connects in plaintext and every mode that requires TLS fails
    /// [`build`](Self::build) with [`Error::FeatureNotSupported`]. A gRPC
    /// endpoint picks TLS through its `https://` scheme, so any mode other
    /// than [`TlsMode::Disable`] fails there with
    /// [`Error::FeatureNotSupported`] too. A query cancel for a TLS session
    /// is sent over TLS. The TLS handshake must finish within
    /// [`login_timeout`](Self::login_timeout) (30 seconds by default; zero
    /// means no limit); the TCP connect itself is not bounded by it.
    ///
    /// The `Connection::connect*` shortcuts and `Connection::new` always
    /// connect in plaintext; use this builder for TLS.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{ConnectionBuilder, Result, TlsConfig, TlsMode};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = ConnectionBuilder::new("hyper.example.com:7483")
    ///         .database("example.hyper")
    ///         .tls(TlsConfig::new(TlsMode::VerifyFull).root_cert("ca.pem"))
    ///         .build()?;
    ///     assert!(conn.is_tls());
    ///     Ok(())
    /// }
    /// ```
    #[must_use]
    pub fn tls(mut self, tls: impl Into<TlsConfig>) -> Self {
        self.tls = tls.into();
        self
    }

    /// Sets the transfer mode for gRPC connections.
    ///
    /// This setting is ignored for TCP connections.
    ///
    /// - `TransferMode::Sync` - All results in one response (simple, 100s timeout)
    /// - `TransferMode::Async` - Header only, fetch results via `GetQueryResult`
    /// - `TransferMode::Adaptive` - First chunk inline, rest streamed (default, recommended)
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{ConnectionBuilder, CreateMode, Result};
    /// use hyperdb_api::grpc::TransferMode;
    ///
    /// fn example() -> Result<()> {
    ///     let conn = ConnectionBuilder::new("https://hyper-server:443")
    ///         .database("example.hyper")
    ///         .transfer_mode(TransferMode::Adaptive)
    ///         .build()?;
    ///     Ok(())
    /// }
    /// ```
    #[must_use]
    pub fn transfer_mode(mut self, mode: crate::grpc::TransferMode) -> Self {
        self.transfer_mode = Some(mode);
        self
    }

    /// Builds and establishes the connection.
    ///
    /// The transport is automatically detected from the endpoint URL:
    /// - `https://` or `http://` → gRPC (read-only)
    /// - `tab.domain://...` or an absolute socket path → Unix domain socket (Unix only)
    /// - `tab.pipe://...` or a `\\host\pipe\name` path → named pipe (Windows only)
    /// - otherwise `host:port` → TCP
    ///
    /// `application_name` and `login_timeout` apply to TCP connections only.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Config`] if the endpoint cannot be parsed, or if the
    ///   TLS settings are invalid or a certificate file cannot be read.
    /// - Returns [`Error::FeatureNotSupported`] for a gRPC endpoint with a create mode other than [`CreateMode::DoNotCreate`],
    ///   or with a [`tls`](Self::tls) mode other than [`TlsMode::Disable`], and for a Unix-socket or named-pipe endpoint
    ///   whose [`tls`](Self::tls) mode requires TLS.
    /// - Returns [`Error::Connection`] or [`Error::Timeout`] if the transport handshake fails.
    /// - Returns [`Error::Tls`] if TLS negotiation or certificate verification fails.
    /// - Returns [`Error::Authentication`] if authentication is rejected.
    /// - Returns [`Error::Server`] or [`Error::Internal`] if the database creation or attach SQL fails.
    pub fn build(self) -> Result<Connection> {
        let transport_type = detect_transport_type(&self.endpoint);

        match transport_type {
            TransportType::Tcp => self.build_tcp(),
            #[cfg(unix)]
            TransportType::UnixSocket => self.build_unix(),
            #[cfg(windows)]
            TransportType::NamedPipe => self.build_named_pipe(),
            TransportType::Grpc => self.build_grpc(),
        }
    }

    /// Build a TCP connection.
    fn build_tcp(self) -> Result<Connection> {
        let mut config: Config = self
            .endpoint
            .parse()
            .map_err(|e| Error::config(format!("invalid endpoint: {e}")))?;

        if let Some(user) = &self.user {
            config = config.with_user(user);
        }

        if let Some(password) = &self.password {
            config = config.with_password(password);
        }

        if let Some(ref app_name) = self.application_name {
            config = config.with_application_name(app_name);
        }

        if let Some(timeout) = self.login_timeout {
            config = config.with_connect_timeout(timeout);
        }
        config = config.with_tls(self.tls);

        let db_path_str = self
            .database
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        let client = hyperdb_api_core::client::Client::connect(&config)?;

        let conn = Connection::from_client(client, db_path_str.clone());
        if let Some(timeout) = self.query_timeout {
            conn.execute_command(&query_timeout_statement(timeout)?)?;
        }

        // Handle database creation (TCP only - gRPC is read-only)
        if let Some(db_path) = db_path_str {
            conn.handle_creation_mode(&db_path, self.create_mode)?;
            conn.attach_and_set_path(&db_path)?;
        }

        Ok(conn)
    }

    /// Build a Unix Domain Socket connection (Unix only).
    #[cfg(unix)]
    fn build_unix(self) -> Result<Connection> {
        use hyperdb_api_core::client::ConnectionEndpoint;

        // Parse the endpoint to get the socket path
        let socket_path = if self.endpoint.starts_with("tab.domain://") {
            // Format: tab.domain://<dir>/domain/<name>
            let endpoint = ConnectionEndpoint::parse(&self.endpoint)
                .map_err(|e| Error::config(format!("invalid Unix socket endpoint: {e}")))?;
            match endpoint {
                ConnectionEndpoint::DomainSocket { directory, name } => directory.join(&name),
                ConnectionEndpoint::Tcp { .. } => {
                    return Err(Error::config("expected Unix domain socket endpoint"));
                }
            }
        } else {
            // Treat as direct socket path
            std::path::PathBuf::from(&self.endpoint)
        };

        let mut config = hyperdb_api_core::client::Config::new();

        if let Some(user) = &self.user {
            config = config.with_user(user);
        }

        if let Some(password) = &self.password {
            config = config.with_password(password);
        }
        config = config.with_tls(self.tls);

        let db_path_str = self
            .database
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // Connect via Unix socket
        let client = hyperdb_api_core::client::Client::connect_unix(&socket_path, &config)?;

        let conn = Connection::from_client(client, db_path_str.clone());
        if let Some(timeout) = self.query_timeout {
            conn.execute_command(&query_timeout_statement(timeout)?)?;
        }

        // Handle database creation
        if let Some(db_path) = db_path_str {
            conn.handle_creation_mode(&db_path, self.create_mode)?;
            conn.attach_and_set_path(&db_path)?;
        }

        Ok(conn)
    }

    /// Build a Windows Named Pipe connection (Windows only).
    #[cfg(windows)]
    fn build_named_pipe(self) -> Result<Connection> {
        use hyperdb_api_core::client::ConnectionEndpoint;

        // Parse the endpoint to get the pipe path
        let pipe_path = if self.endpoint.starts_with("tab.pipe://") {
            // Format: tab.pipe://<host>/pipe/<name>
            let endpoint = ConnectionEndpoint::parse(&self.endpoint)
                .map_err(|e| Error::config(format!("invalid named pipe endpoint: {e}")))?;
            match endpoint {
                ConnectionEndpoint::NamedPipe { host, name } => {
                    format!(r"\\{host}\pipe\{name}")
                }
                _ => return Err(Error::config("expected named pipe endpoint")),
            }
        } else {
            // Treat as direct pipe path (e.g., \\.\pipe\hyper-12345)
            self.endpoint.clone()
        };

        let mut config = hyperdb_api_core::client::Config::new();

        if let Some(user) = &self.user {
            config = config.with_user(user);
        }

        if let Some(password) = &self.password {
            config = config.with_password(password);
        }
        config = config.with_tls(self.tls);

        let db_path_str = self
            .database
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // Connect via Named Pipe
        let client = hyperdb_api_core::client::Client::connect_named_pipe(&pipe_path, &config)?;

        let conn = Connection::from_client(client, db_path_str.clone());
        if let Some(timeout) = self.query_timeout {
            conn.execute_command(&query_timeout_statement(timeout)?)?;
        }

        // Handle database creation
        if let Some(db_path) = db_path_str {
            conn.handle_creation_mode(&db_path, self.create_mode)?;
            conn.attach_and_set_path(&db_path)?;
        }

        Ok(conn)
    }

    /// Build a gRPC connection.
    fn build_grpc(self) -> Result<Connection> {
        if self.query_timeout.is_some() {
            return Err(Error::feature_not_supported(
                "query_timeout is not supported on gRPC connections",
            ));
        }
        if self.tls.mode() != TlsMode::Disable {
            return Err(Error::feature_not_supported(
                "tls is not supported on gRPC connections; use an https:// endpoint for TLS",
            ));
        }

        // Validate create_mode - gRPC is read-only
        if self.create_mode != CreateMode::DoNotCreate {
            return Err(Error::feature_not_supported(
                "gRPC transport is read-only. Use CreateMode::DoNotCreate for gRPC connections.",
            ));
        }

        let db_path_str = self
            .database
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // Build gRPC config
        let mut grpc_config = hyperdb_api_core::client::grpc::GrpcConfig::new(&self.endpoint);

        if let Some(ref db_path) = db_path_str {
            grpc_config = grpc_config.database(db_path);
        }

        // Apply transfer mode if specified
        if let Some(mode) = self.transfer_mode {
            grpc_config = grpc_config.transfer_mode(mode);
        }

        // Connect via gRPC
        let transport = Transport::connect_grpc(grpc_config)?;

        Ok(Connection::from_transport(transport, db_path_str))
    }
}
