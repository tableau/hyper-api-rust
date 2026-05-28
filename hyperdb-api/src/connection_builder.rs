// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::connection::{Connection, CreateMode};
use crate::error::{Error, Result};
use crate::transport::{detect_transport_type, Transport, TransportType};
use hyperdb_api_core::client::Config;

/// A builder for creating database connections.
///
/// This provides a flexible way to configure a connection with various options
/// like authentication, database creation mode, and transport settings.
///
/// # Transport Auto-Detection
///
/// The transport is automatically detected from the endpoint URL:
/// - `https://` or `http://` → gRPC transport (read-only)
/// - Otherwise → TCP transport (e.g., `localhost:7483`)
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
    /// Query timeout — cancel queries that exceed this duration.
    query_timeout: Option<Duration>,
    /// Application name sent to the server during connection startup.
    application_name: Option<String>,
    /// Transfer mode for gRPC connections (ignored for TCP)
    transfer_mode: Option<hyperdb_api_core::client::grpc::TransferMode>,
}

impl Default for ConnectionBuilder {
    fn default() -> Self {
        Self::new("localhost:7483")
    }
}

impl ConnectionBuilder {
    /// Creates a new builder for the given endpoint.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The server endpoint (host:port).
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
        }
    }

    #[must_use]
    /// Sets the database path.
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

    #[must_use]
    /// Sets the username for authentication.
    ///
    /// Default is "`tableau_internal_user`".
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    #[must_use]
    /// Sets the password for authentication.
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Sets the login timeout.
    #[must_use]
    pub fn login_timeout(mut self, timeout: Duration) -> Self {
        self.login_timeout = Some(timeout);
        self
    }

    /// Sets the query timeout.
    ///
    /// Queries that exceed this duration will be cancelled automatically.
    /// Default is no timeout (queries run until completion).
    #[must_use]
    pub fn query_timeout(mut self, timeout: Duration) -> Self {
        self.query_timeout = Some(timeout);
        self
    }

    #[must_use]
    /// Sets the application name sent to the server.
    ///
    /// This appears in server logs and can be used for monitoring.
    pub fn application_name(mut self, name: impl Into<String>) -> Self {
        self.application_name = Some(name.into());
        self
    }

    #[must_use]
    /// Convenience method to set user and password at once.
    pub fn auth(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self.password = Some(password.into());
        self
    }

    #[must_use]
    /// Convenience method to create a new database.
    pub fn create_new_database(mut self, database_path: impl AsRef<Path>) -> Self {
        self.database = Some(database_path.as_ref().to_path_buf());
        self.create_mode = CreateMode::Create;
        self
    }

    #[must_use]
    /// Convenience method to create database if it doesn't exist.
    pub fn create_or_open_database(mut self, database_path: impl AsRef<Path>) -> Self {
        self.database = Some(database_path.as_ref().to_path_buf());
        self.create_mode = CreateMode::CreateIfNotExists;
        self
    }

    #[must_use]
    /// Convenience method to open an existing database.
    pub fn open_database(mut self, database_path: impl AsRef<Path>) -> Self {
        self.database = Some(database_path.as_ref().to_path_buf());
        self.create_mode = CreateMode::DoNotCreate;
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
    pub fn transfer_mode(mut self, mode: hyperdb_api_core::client::grpc::TransferMode) -> Self {
        self.transfer_mode = Some(mode);
        self
    }

    /// Builds and establishes the connection.
    ///
    /// The transport is automatically detected from the endpoint URL:
    /// - `https://` or `http://` → gRPC transport
    /// - Otherwise → TCP transport
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails or if database creation fails.
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

        let db_path_str = self
            .database
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        let client = hyperdb_api_core::client::Client::connect(&config)?;

        let conn = Connection::from_client(client, db_path_str.clone());

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
                    return Err(Error::config("expected Unix domain socket endpoint"))
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

        let db_path_str = self
            .database
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // Connect via Unix socket
        let client = hyperdb_api_core::client::Client::connect_unix(&socket_path, &config)?;

        let conn = Connection::from_client(client, db_path_str.clone());

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

        let db_path_str = self
            .database
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // Connect via Named Pipe
        let client = hyperdb_api_core::client::Client::connect_named_pipe(&pipe_path, &config)?;

        let conn = Connection::from_client(client, db_path_str.clone());

        // Handle database creation
        if let Some(db_path) = db_path_str {
            conn.handle_creation_mode(&db_path, self.create_mode)?;
            conn.attach_and_set_path(&db_path)?;
        }

        Ok(conn)
    }

    /// Build a gRPC connection.
    fn build_grpc(self) -> Result<Connection> {
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
