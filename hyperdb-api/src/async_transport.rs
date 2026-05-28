// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async transport abstraction for unified TCP/gRPC connectivity.
//!
//! This module provides [`AsyncTransport`], the async version of the internal
//! transport layer that supports both TCP and gRPC connections.

#![expect(
    dead_code,
    reason = "infrastructure for AsyncConnection; not every variant is reached on every feature combo"
)]

use crate::arrow_result::ArrowRowset;
use crate::async_result::AsyncRowset;
use crate::error::{Error, Result};
use crate::result::DEFAULT_BINARY_CHUNK_SIZE;
use crate::transport::TransportType;

/// Async transport enum holding the actual async client.
pub(crate) enum AsyncTransport {
    Tcp(AsyncTcpTransport),
    Grpc(Box<AsyncGrpcTransport>),
}

/// Async TCP transport using hyper-client's `AsyncClient`.
pub(crate) struct AsyncTcpTransport {
    pub(crate) client: hyperdb_api_core::client::AsyncClient,
}

/// Async gRPC transport using hyper-client's `GrpcClient`.
pub(crate) struct AsyncGrpcTransport {
    pub(crate) client: hyperdb_api_core::client::grpc::GrpcClient,
    pub(crate) config: hyperdb_api_core::client::grpc::GrpcConfig,
}

/// Parse an endpoint string like "host:port" into host and port.
fn parse_endpoint(endpoint: &str) -> (&str, u16) {
    if let Some(colon_pos) = endpoint.rfind(':') {
        let host = &endpoint[..colon_pos];
        let port_str = &endpoint[colon_pos + 1..];
        let port = port_str.parse::<u16>().unwrap_or(7483);
        (host, port)
    } else {
        (endpoint, 7483)
    }
}

impl AsyncTransport {
    /// Returns the transport type.
    pub(crate) fn transport_type(&self) -> TransportType {
        match self {
            AsyncTransport::Tcp(_) => TransportType::Tcp,
            AsyncTransport::Grpc(_) => TransportType::Grpc,
        }
    }

    /// Returns true if this transport supports write operations.
    pub(crate) fn supports_writes(&self) -> bool {
        match self {
            AsyncTransport::Tcp(_) => true,
            AsyncTransport::Grpc(_) => false,
        }
    }

    /// Connect using TCP transport (async).
    pub(crate) async fn connect_tcp(endpoint: &str) -> Result<Self> {
        let (host, port) = parse_endpoint(endpoint);
        let config = hyperdb_api_core::client::Config::new()
            .with_host(host)
            .with_port(port)
            .with_user("tableau_internal_user"); // Default user like sync Connection
        let client = hyperdb_api_core::client::AsyncClient::connect(&config).await?;
        Ok(AsyncTransport::Tcp(AsyncTcpTransport { client }))
    }

    /// Connect using TCP transport with authentication (async).
    pub(crate) async fn connect_tcp_with_auth(
        endpoint: &str,
        user: &str,
        password: &str,
    ) -> Result<Self> {
        let (host, port) = parse_endpoint(endpoint);
        let config = hyperdb_api_core::client::Config::new()
            .with_host(host)
            .with_port(port)
            .with_user(user)
            .with_password(password);
        let client = hyperdb_api_core::client::AsyncClient::connect(&config).await?;
        Ok(AsyncTransport::Tcp(AsyncTcpTransport { client }))
    }

    /// Connect using gRPC transport (async).
    pub(crate) async fn connect_grpc(
        config: hyperdb_api_core::client::grpc::GrpcConfig,
    ) -> Result<Self> {
        let client = hyperdb_api_core::client::grpc::GrpcClient::connect(config.clone()).await?;
        Ok(AsyncTransport::Grpc(Box::new(AsyncGrpcTransport {
            client,
            config,
        })))
    }

    /// Auto-detect transport type and connect (async).
    ///
    /// - `https://` or `http://` prefix → gRPC
    /// - `tab.domain://` prefix or absolute path (Unix only) → UDS
    /// - `tab.pipe://` prefix or `\\` prefix (Windows only) → Named Pipe
    /// - Otherwise → TCP (`PostgreSQL` wire protocol)
    pub(crate) async fn connect(endpoint: &str, database: Option<&str>) -> Result<Self> {
        use crate::transport::detect_transport_type;

        match detect_transport_type(endpoint) {
            TransportType::Grpc => {
                let mut config = hyperdb_api_core::client::grpc::GrpcConfig::new(endpoint);
                if let Some(db) = database {
                    config = config.database(db);
                }
                Self::connect_grpc(config).await
            }
            #[cfg(unix)]
            TransportType::UnixSocket => Self::connect_unix(endpoint).await,
            #[cfg(windows)]
            TransportType::NamedPipe => Self::connect_named_pipe(endpoint).await,
            TransportType::Tcp => Self::connect_tcp(endpoint).await,
        }
    }

    /// Connect using a Unix Domain Socket (async, Unix only).
    ///
    /// Accepts either a `tab.domain://<dir>/domain/<name>` URI or a raw
    /// absolute path to the socket file.
    #[cfg(unix)]
    pub(crate) async fn connect_unix(endpoint: &str) -> Result<Self> {
        use hyperdb_api_core::client::ConnectionEndpoint;

        let socket_path = if endpoint.starts_with("tab.domain://") {
            let parsed = ConnectionEndpoint::parse(endpoint)
                .map_err(|e| Error::config(format!("invalid Unix socket endpoint: {e}")))?;
            match parsed {
                ConnectionEndpoint::DomainSocket { directory, name } => directory.join(&name),
                ConnectionEndpoint::Tcp { .. } => {
                    return Err(Error::config("expected Unix domain socket endpoint"));
                }
            }
        } else {
            std::path::PathBuf::from(endpoint)
        };

        let config = hyperdb_api_core::client::Config::new().with_user("tableau_internal_user");
        let client =
            hyperdb_api_core::client::AsyncClient::connect_unix(&socket_path, &config).await?;
        Ok(AsyncTransport::Tcp(AsyncTcpTransport { client }))
    }

    /// Connect using a Windows Named Pipe (async, Windows only).
    ///
    /// Accepts either a `tab.pipe://<host>/pipe/<name>` URI or a raw
    /// `\\<host>\pipe\<name>` path.
    #[cfg(windows)]
    pub(crate) async fn connect_named_pipe(endpoint: &str) -> Result<Self> {
        use hyperdb_api_core::client::ConnectionEndpoint;

        let pipe_path = if endpoint.starts_with("tab.pipe://") {
            let parsed = ConnectionEndpoint::parse(endpoint)
                .map_err(|e| Error::config(format!("invalid named pipe endpoint: {e}")))?;
            match parsed {
                ConnectionEndpoint::NamedPipe { host, name } => {
                    format!(r"\\{host}\pipe\{name}")
                }
                _ => return Err(Error::config("expected named pipe endpoint")),
            }
        } else {
            endpoint.to_string()
        };

        let config = hyperdb_api_core::client::Config::new().with_user("tableau_internal_user");
        let client =
            hyperdb_api_core::client::AsyncClient::connect_named_pipe(&pipe_path, &config).await?;
        Ok(AsyncTransport::Tcp(AsyncTcpTransport { client }))
    }

    /// Execute a command (DDL/DML) - returns affected row count (async).
    pub(crate) async fn execute_command(&self, sql: &str) -> Result<u64> {
        match self {
            AsyncTransport::Tcp(tcp) => Ok(tcp.client.exec(sql).await?),
            AsyncTransport::Grpc(_) => Err(Error::feature_not_supported(
                "gRPC transport is read-only. Write operations (INSERT, UPDATE, DELETE, DDL) \
                 are not yet supported over gRPC. Use a TCP connection for write operations.",
            )),
        }
    }

    /// Execute a query returning raw Arrow IPC bytes (async).
    ///
    /// TCP uses `COPY ... TO STDOUT WITH (FORMAT ARROWSTREAM)` under the
    /// hood so the response arrives pre-encoded as Arrow IPC — this is the
    /// same shape as the sync Connection's TCP Arrow path.
    pub(crate) async fn execute_query_to_arrow(&self, sql: &str) -> Result<bytes::Bytes> {
        match self {
            AsyncTransport::Tcp(tcp) => {
                let copy_sql = format!("COPY ({sql}) TO STDOUT WITH (FORMAT ARROWSTREAM)");
                let bytes = tcp.client.copy_out(&copy_sql).await?;
                Ok(bytes::Bytes::from(bytes))
            }
            AsyncTransport::Grpc(grpc) => {
                // gRPC needs &mut to mutate the tonic channel; spin up a
                // per-call clone rather than locking the transport. The
                // config clone is cheap (Arc inside).
                let mut client =
                    hyperdb_api_core::client::grpc::GrpcClient::connect(grpc.config.clone())
                        .await?;
                Ok(client.execute_query_to_arrow(sql).await?)
            }
        }
    }

    /// Execute a query returning a streaming `AsyncRowset` (async).
    ///
    /// For TCP this wraps a genuinely streaming `AsyncQueryStream`.
    /// For gRPC this materializes the full Arrow IPC response into an
    /// `ArrowRowset` — a deliberate shortcut that mirrors what sync would
    /// do if it did not have a runtime to drive `GrpcChunkStreamSync` with
    /// `block_on`. The gRPC path can be upgraded to lazy chunked streaming
    /// in a follow-up.
    pub(crate) async fn execute_query_streaming(&self, sql: &str) -> Result<AsyncRowset<'_>> {
        match self {
            AsyncTransport::Tcp(tcp) => {
                let stream = tcp
                    .client
                    .query_streaming(sql, DEFAULT_BINARY_CHUNK_SIZE)
                    .await?;
                Ok(AsyncRowset::new(stream))
            }
            AsyncTransport::Grpc(grpc) => {
                // Clone the config to get a mutable client we can execute on
                // — GrpcClient methods require &mut self and the transport
                // holds a shared reference. Cloning the channel is cheap
                // (Arc inside).
                let mut client =
                    hyperdb_api_core::client::grpc::GrpcClient::connect(grpc.config.clone())
                        .await?;
                let ipc_bytes = client.execute_query_to_arrow(sql).await?;
                let arrow_rowset = ArrowRowset::from_bytes(ipc_bytes)?;
                Ok(AsyncRowset::from_arrow(arrow_rowset))
            }
        }
    }

    /// Cancel the currently running query (async).
    pub(crate) async fn cancel(&self) -> Result<()> {
        match self {
            AsyncTransport::Tcp(tcp) => {
                tcp.client.cancel().await?;
                Ok(())
            }
            AsyncTransport::Grpc(_) => {
                // gRPC cancellation would be handled differently
                Err(Error::feature_not_supported(
                    "Query cancellation not supported over gRPC",
                ))
            }
        }
    }

    /// Close the transport (async).
    pub(crate) async fn close(self) -> Result<()> {
        match self {
            AsyncTransport::Tcp(tcp) => {
                tcp.client.close().await?;
                Ok(())
            }
            AsyncTransport::Grpc(_) => {
                // gRPC client doesn't need explicit close
                Ok(())
            }
        }
    }

    /// Returns a reference to the async TCP client if using TCP transport.
    pub(crate) fn async_tcp_client(&self) -> Option<&hyperdb_api_core::client::AsyncClient> {
        match self {
            AsyncTransport::Tcp(tcp) => Some(&tcp.client),
            AsyncTransport::Grpc(_) => None,
        }
    }
}
