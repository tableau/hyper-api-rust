// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Fluent builder for [`AsyncConnection`].
//!
//! Mirrors [`ConnectionBuilder`](crate::ConnectionBuilder) one-for-one,
//! differing only in that `build()` and the transport-specific builders
//! are `async`. The field set and defaults are identical so users can
//! swap between the two with minimal friction.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::async_connection::AsyncConnection;
use crate::async_transport::AsyncTransport;
use crate::connection::CreateMode;
use crate::error::{Error, Result};
use crate::transport::{detect_transport_type, TransportType};
use hyperdb_api_core::client::{AsyncClient, Config};

/// An async builder for creating database connections.
///
/// See [`ConnectionBuilder`](crate::ConnectionBuilder) for the sync
/// equivalent and the full contract. All setters are identical; only
/// the terminal `build()` calls are async.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{AsyncConnectionBuilder, CreateMode, Result};
///
/// #[tokio::main]
/// async fn main() -> Result<()> {
///     let conn = AsyncConnectionBuilder::new("localhost:7483")
///         .database("example.hyper")
///         .create_mode(CreateMode::CreateIfNotExists)
///         .auth("myuser", "mypassword")
///         .build()
///         .await?;
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone)]
pub struct AsyncConnectionBuilder {
    endpoint: String,
    database: Option<PathBuf>,
    create_mode: CreateMode,
    user: Option<String>,
    password: Option<String>,
    login_timeout: Option<Duration>,
    query_timeout: Option<Duration>,
    application_name: Option<String>,
    transfer_mode: Option<hyperdb_api_core::client::grpc::TransferMode>,
}

impl Default for AsyncConnectionBuilder {
    fn default() -> Self {
        Self::new("localhost:7483")
    }
}

impl AsyncConnectionBuilder {
    /// Creates a new builder for the given endpoint.
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
            transfer_mode: None,
        }
    }

    #[must_use]
    /// Sets the database path.
    pub fn database(mut self, path: impl AsRef<Path>) -> Self {
        self.database = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the database creation mode.
    #[must_use]
    pub fn create_mode(mut self, mode: CreateMode) -> Self {
        self.create_mode = mode;
        self
    }

    #[must_use]
    /// Sets the username for authentication.
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
    #[must_use]
    pub fn query_timeout(mut self, timeout: Duration) -> Self {
        self.query_timeout = Some(timeout);
        self
    }

    #[must_use]
    /// Sets the application name sent to the server.
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

    /// Sets the transfer mode for gRPC connections (ignored for TCP).
    #[must_use]
    pub fn transfer_mode(mut self, mode: hyperdb_api_core::client::grpc::TransferMode) -> Self {
        self.transfer_mode = Some(mode);
        self
    }

    /// Builds and establishes the connection (async).
    ///
    /// Transport is auto-detected from the endpoint URL:
    /// - `https://` / `http://` → gRPC
    /// - `tab.domain://` → Unix domain socket (Unix only)
    /// - `tab.pipe://` → named pipe (Windows only)
    /// - otherwise → TCP
    ///
    /// # Errors
    ///
    /// - Returns [`Error::Io`] or [`Error::Connection`] if the transport
    ///   handshake fails (TCP refused, TLS rejected, named-pipe not
    ///   found, gRPC channel setup failure).
    /// - Returns [`Error::Authentication`] if authentication is rejected.
    /// - Returns [`Error::Server`] if the `CreateMode` SQL is rejected
    ///   for a builder that configured a database path.
    pub async fn build(self) -> Result<AsyncConnection> {
        let transport_type = detect_transport_type(&self.endpoint);
        match transport_type {
            TransportType::Tcp => self.build_tcp().await,
            #[cfg(unix)]
            TransportType::UnixSocket => self.build_unix().await,
            #[cfg(windows)]
            TransportType::NamedPipe => self.build_named_pipe().await,
            TransportType::Grpc => self.build_grpc().await,
        }
    }

    /// Build a TCP connection (async).
    async fn build_tcp(self) -> Result<AsyncConnection> {
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

        let client = AsyncClient::connect(&config).await?;
        let conn = AsyncConnection::from_async_client(client, db_path_str.clone());

        if let Some(db_path) = db_path_str {
            conn.handle_creation_mode_public(&db_path, self.create_mode)
                .await?;
            conn.attach_and_set_path_public(&db_path).await?;
        }

        Ok(conn)
    }

    /// Build a Unix domain socket connection (async, Unix only).
    #[cfg(unix)]
    async fn build_unix(self) -> Result<AsyncConnection> {
        use hyperdb_api_core::client::ConnectionEndpoint;

        let socket_path = if self.endpoint.starts_with("tab.domain://") {
            let endpoint = ConnectionEndpoint::parse(&self.endpoint)
                .map_err(|e| Error::config(format!("invalid Unix socket endpoint: {e}")))?;
            match endpoint {
                ConnectionEndpoint::DomainSocket { directory, name } => directory.join(&name),
                ConnectionEndpoint::Tcp { .. } => {
                    return Err(Error::config("expected Unix domain socket endpoint"))
                }
            }
        } else {
            std::path::PathBuf::from(&self.endpoint)
        };

        let mut config = Config::new();
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

        let client = AsyncClient::connect_unix(&socket_path, &config).await?;
        let conn = AsyncConnection::from_async_client(client, db_path_str.clone());

        if let Some(db_path) = db_path_str {
            conn.handle_creation_mode_public(&db_path, self.create_mode)
                .await?;
            conn.attach_and_set_path_public(&db_path).await?;
        }

        Ok(conn)
    }

    /// Build a Windows Named Pipe connection (async, Windows only).
    #[cfg(windows)]
    async fn build_named_pipe(self) -> Result<AsyncConnection> {
        use hyperdb_api_core::client::ConnectionEndpoint;

        let pipe_path = if self.endpoint.starts_with("tab.pipe://") {
            let endpoint = ConnectionEndpoint::parse(&self.endpoint)
                .map_err(|e| Error::config(format!("invalid named pipe endpoint: {e}")))?;
            match endpoint {
                ConnectionEndpoint::NamedPipe { host, name } => {
                    format!(r"\\{host}\pipe\{name}")
                }
                _ => return Err(Error::config("expected named pipe endpoint")),
            }
        } else {
            self.endpoint.clone()
        };

        let mut config = Config::new();
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

        let client = AsyncClient::connect_named_pipe(&pipe_path, &config).await?;
        let conn = AsyncConnection::from_async_client(client, db_path_str.clone());

        if let Some(db_path) = db_path_str {
            conn.handle_creation_mode_public(&db_path, self.create_mode)
                .await?;
            conn.attach_and_set_path_public(&db_path).await?;
        }

        Ok(conn)
    }

    /// Build a gRPC connection (async).
    async fn build_grpc(self) -> Result<AsyncConnection> {
        if self.create_mode != CreateMode::DoNotCreate {
            return Err(Error::feature_not_supported(
                "gRPC transport is read-only. Use CreateMode::DoNotCreate for gRPC connections.",
            ));
        }

        let db_path_str = self
            .database
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        let mut grpc_config = hyperdb_api_core::client::grpc::GrpcConfig::new(&self.endpoint);
        if let Some(ref db_path) = db_path_str {
            grpc_config = grpc_config.database(db_path);
        }
        if let Some(mode) = self.transfer_mode {
            grpc_config = grpc_config.transfer_mode(mode);
        }

        let transport = AsyncTransport::connect_grpc(grpc_config).await?;
        Ok(AsyncConnection::from_transport(transport, db_path_str))
    }
}
