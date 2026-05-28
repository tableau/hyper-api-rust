// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Internal transport abstraction for unified TCP/gRPC connectivity.
//!
//! This module is internal and not part of the public API.

#![expect(
    dead_code,
    reason = "infrastructure for direct transport creation; accessors used under specific feature combos"
)]

use crate::error::{Error, Result};

/// Transport type indicator (public for introspection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportType {
    /// TCP connection using `PostgreSQL` wire protocol.
    Tcp,
    /// Unix Domain Socket connection (Unix only).
    #[cfg(unix)]
    UnixSocket,
    /// Windows Named Pipe connection (Windows only).
    #[cfg(windows)]
    NamedPipe,
    /// gRPC connection using HTTP/2 and Arrow IPC format.
    Grpc,
}

impl TransportType {
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "signature kept for API consistency with the trait family that unifies Copy and non-Copy implementers"
    )]
    /// Returns a human-readable name for this transport type.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            TransportType::Tcp => "TCP",
            #[cfg(unix)]
            TransportType::UnixSocket => "Unix Socket",
            #[cfg(windows)]
            TransportType::NamedPipe => "Named Pipe",
            TransportType::Grpc => "gRPC",
        }
    }
}

impl std::fmt::Display for TransportType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Detects transport type from endpoint string.
///
/// - `https://` or `http://` prefix → gRPC
/// - `tab.domain://` prefix → Unix Domain Socket (Unix only)
/// - Absolute path starting with `/` → Unix Domain Socket (Unix only)
/// - `tab.pipe://` prefix → Windows Named Pipe (Windows only)
/// - `\\` prefix → Windows Named Pipe (Windows only)
/// - Otherwise → TCP (`PostgreSQL` wire protocol)
pub(crate) fn detect_transport_type(endpoint: &str) -> TransportType {
    if endpoint.starts_with("https://") || endpoint.starts_with("http://") {
        TransportType::Grpc
    } else {
        #[cfg(unix)]
        {
            if endpoint.starts_with("tab.domain://") || endpoint.starts_with('/') {
                return TransportType::UnixSocket;
            }
        }
        #[cfg(windows)]
        {
            if endpoint.starts_with("tab.pipe://") || endpoint.starts_with(r"\\") {
                return TransportType::NamedPipe;
            }
        }
        TransportType::Tcp
    }
}

/// Internal transport enum holding the actual client.
pub(crate) enum Transport {
    Tcp(Box<TcpTransport>),
    Grpc(Box<GrpcTransport>),
}

/// TCP transport using hyper-client.
pub(crate) struct TcpTransport {
    pub(crate) client: hyperdb_api_core::client::Client,
}

/// gRPC transport using hyper-client's gRPC module.
pub(crate) struct GrpcTransport {
    pub(crate) client: hyperdb_api_core::client::grpc::GrpcClientSync,
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

impl Transport {
    /// Returns the transport type.
    pub(crate) fn transport_type(&self) -> TransportType {
        match self {
            Transport::Tcp(_) => TransportType::Tcp,
            Transport::Grpc(_) => TransportType::Grpc,
        }
    }

    /// Returns true if this transport supports write operations.
    pub(crate) fn supports_writes(&self) -> bool {
        match self {
            Transport::Tcp(_) => true,
            Transport::Grpc(_) => false, // TODO: Server capability check in future
        }
    }

    /// Connect using TCP transport.
    pub(crate) fn connect_tcp(endpoint: &str) -> Result<Self> {
        let (host, port) = parse_endpoint(endpoint);
        let config = hyperdb_api_core::client::Config::new()
            .with_host(host)
            .with_port(port);
        let client = hyperdb_api_core::client::Client::connect(&config)?;
        Ok(Transport::Tcp(Box::new(TcpTransport { client })))
    }

    /// Connect using TCP transport with authentication.
    pub(crate) fn connect_tcp_with_auth(
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
        let client = hyperdb_api_core::client::Client::connect(&config)?;
        Ok(Transport::Tcp(Box::new(TcpTransport { client })))
    }

    /// Connect using gRPC transport.
    pub(crate) fn connect_grpc(config: hyperdb_api_core::client::grpc::GrpcConfig) -> Result<Self> {
        let client = hyperdb_api_core::client::grpc::GrpcClientSync::connect(config.clone())?;
        Ok(Transport::Grpc(Box::new(GrpcTransport { client, config })))
    }

    /// Execute a command (DDL/DML) - returns affected row count.
    pub(crate) fn execute_command(&self, sql: &str) -> Result<u64> {
        match self {
            Transport::Tcp(tcp) => Ok(tcp.client.exec(sql)?),
            Transport::Grpc(_) => Err(Error::feature_not_supported(
                "gRPC transport is read-only. Write operations (INSERT, UPDATE, DELETE, DDL) \
                 are not yet supported over gRPC. Use a TCP connection for write operations.",
            )),
        }
    }

    /// Execute a query returning raw Arrow IPC bytes.
    pub(crate) fn execute_query_to_arrow(&self, sql: &str) -> Result<bytes::Bytes> {
        match self {
            Transport::Tcp(tcp) => {
                let copy_query = format!("COPY ({sql}) TO STDOUT WITH (format arrowstream)");
                // `Bytes::from(Vec<u8>)` takes ownership without copying.
                Ok(bytes::Bytes::from(tcp.client.copy_out(&copy_query)?))
            }
            Transport::Grpc(grpc) => {
                // gRPC client needs mutable access - we use a workaround here
                // by creating a new client for each query. This is acceptable
                // for Arrow queries which are typically infrequent.
                let mut client =
                    hyperdb_api_core::client::grpc::GrpcClientSync::connect(grpc.config.clone())?;
                Ok(client.execute_query_to_arrow(sql)?)
            }
        }
    }
}
