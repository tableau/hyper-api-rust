// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! High-level asynchronous client for Hyper database.
//!
//! This module provides [`AsyncClient`], the async version of [`Client`](crate::client::Client).
//! It uses tokio for async I/O operations.

use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

#[cfg(unix)]
use tokio::net::UnixStream;

use super::async_connection::AsyncRawConnection;
use super::async_stream::AsyncStream;
use super::async_stream_query::AsyncQueryStream;
use super::cancel::Cancellable;
use super::config::Config;
use super::endpoint::ConnectionEndpoint;
use super::error::{Error, Result};
use super::notice::{Notice, NoticeReceiver};
use super::row::{Row, StreamRow};

use crate::protocol::message::Message;

/// An asynchronous client for Hyper database.
///
/// This is the async equivalent of [`Client`](crate::client::Client), designed for use
/// in tokio-based async applications. All I/O operations are non-blocking.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api_core::client::{AsyncClient, Config};
///
/// #[tokio::main]
/// async fn main() -> hyperdb_api_core::client::Result<()> {
///     let config = Config::new()
///         .with_host("localhost")
///         .with_port(7483)
///         .with_database("test.hyper");
///
///     let client = AsyncClient::connect(&config).await?;
///     let rows = client.query("SELECT 1").await?;
///     client.close().await?;
///     Ok(())
/// }
/// ```
pub struct AsyncClient {
    /// The underlying async connection, protected by a mutex for concurrent access.
    connection: Arc<Mutex<AsyncRawConnection<AsyncStream>>>,
    /// Backend process ID (for cancel requests).
    process_id: i32,
    /// Secret key for authenticating cancel requests.
    secret_key: i32,
    /// Connection endpoint for cancel requests and reconnection.
    endpoint: ConnectionEndpoint,
    /// Optional notice receiver callback for server notices/warnings.
    notice_receiver: Option<Arc<NoticeReceiver>>,
}

impl std::fmt::Debug for AsyncClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncClient")
            .field("process_id", &self.process_id)
            .field("secret_key", &self.secret_key)
            .field("endpoint", &self.endpoint)
            .field(
                "notice_receiver",
                &self.notice_receiver.as_ref().map(|_| "<callback>"),
            )
            .finish_non_exhaustive()
    }
}

impl AsyncClient {
    /// Connects to a Hyper server using the given configuration (async).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::{AsyncClient, Config};
    /// # async fn example() -> hyperdb_api_core::client::Result<()> {
    /// let config = Config::new()
    ///     .with_host("localhost")
    ///     .with_port(7483)
    ///     .with_database("test.hyper");
    ///
    /// let client = AsyncClient::connect(&config).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the TCP connection cannot be
    ///   established to `config.host():config.port()`.
    /// - Propagates any [`Error`] from the startup handshake —
    ///   [`Error`] (auth) for missing/wrong credentials,
    ///   [`Error`] (server) for server-side startup errors, [`Error`] (protocol)
    ///   for out-of-sequence messages, or [`Error`] (I/O) for wire
    ///   failure.
    pub async fn connect(config: &Config) -> Result<Self> {
        info!(
            target: "hyperdb_api",
            host = %config.host(),
            port = config.port(),
            user = config.user().unwrap_or("(default)"),
            database = config.database().unwrap_or("(none)"),
            "connection-parameters"
        );

        let endpoint = ConnectionEndpoint::tcp(config.host(), config.port());
        let addr = format!("{}:{}", config.host(), config.port());
        let tcp_stream = TcpStream::connect(&addr).await.map_err(|e| {
            warn!(target: "hyperdb_api", %addr, error = %e, "connection-failed");
            Error::connection(format!("failed to connect to {addr}: {e}"))
        })?;

        // Set TCP options. See the sync mirror in
        // [`super::client::Client::connect`] for the full rationale and
        // empirical knee analysis. We bump `SO_RCVBUF` / `SO_SNDBUF` to
        // 4 MiB (Windows default ~64 KiB throttles loopback throughput;
        // Linux auto-tunes higher).
        tcp_stream.set_nodelay(true).ok();
        let sock = socket2::SockRef::from(&tcp_stream);
        sock.set_recv_buffer_size(4 * 1024 * 1024).ok();
        sock.set_send_buffer_size(4 * 1024 * 1024).ok();
        // TCP keepalive: detect a half-open peer (laptop sleep, network blip,
        // a hyperd that vanished without a FIN) in ~90s instead of the 2h OS
        // idle default. See the rationale on `super::client::apply_tcp_keepalive`
        // (the sync mirror). Best-effort: a rejected knob leaves OS defaults.
        {
            let keepalive = socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(60))
                .with_interval(std::time::Duration::from_secs(10));
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let keepalive = keepalive.with_retries(3);
            sock.set_tcp_keepalive(&keepalive).ok();
        }

        let stream = AsyncStream::tcp(tcp_stream);
        let mut connection = AsyncRawConnection::new(stream);

        // Perform startup with authentication
        let params = config.startup_params();
        let params_ref: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, *v)).collect();
        connection.startup(&params_ref, config.password()).await?;

        let process_id = connection.process_id();
        let secret_key = connection.secret_key();

        debug!(
            target: "hyperdb_api",
            process_id,
            "connection-established"
        );

        Ok(AsyncClient {
            connection: Arc::new(Mutex::new(connection)),
            process_id,
            secret_key,
            endpoint,
            notice_receiver: None,
        })
    }

    /// Connects to a Hyper server via Unix Domain Socket (async, Unix only).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::{AsyncClient, Config};
    /// # use std::path::Path;
    /// # async fn example() -> hyperdb_api_core::client::Result<()> {
    /// let socket_path = Path::new("/tmp/hyper/.s.PGSQL.12345");
    /// let config = Config::new().with_database("test.hyper");
    /// let client = AsyncClient::connect_unix(socket_path, &config).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the Unix domain socket cannot
    ///   be connected.
    /// - Propagates any error from the startup handshake (see
    ///   [`Self::connect`]).
    #[cfg(unix)]
    pub async fn connect_unix(
        socket_path: impl AsRef<std::path::Path>,
        config: &Config,
    ) -> Result<Self> {
        use std::path::Path;

        let path = socket_path.as_ref();
        info!(
            target: "hyperdb_api",
            socket_path = %path.display(),
            user = config.user().unwrap_or("(default)"),
            database = config.database().unwrap_or("(none)"),
            "connection-parameters-unix"
        );

        let unix_stream = UnixStream::connect(path).await.map_err(|e| {
            warn!(target: "hyperdb_api", socket_path = %path.display(), error = %e, "connection-failed");
            Error::connection(format!("failed to connect to unix socket {}: {}", path.display(), e))
        })?;

        // Parse endpoint from socket path
        let directory = path.parent().unwrap_or(Path::new("/"));
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("socket");
        let endpoint = ConnectionEndpoint::domain_socket(directory, name);

        let stream = AsyncStream::unix(unix_stream);
        let mut connection = AsyncRawConnection::new(stream);

        // Perform startup with authentication
        let params = config.startup_params();
        let params_ref: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, *v)).collect();
        connection.startup(&params_ref, config.password()).await?;

        let process_id = connection.process_id();
        let secret_key = connection.secret_key();

        debug!(
            target: "hyperdb_api",
            process_id,
            "connection-established-unix"
        );

        Ok(AsyncClient {
            connection: Arc::new(Mutex::new(connection)),
            process_id,
            secret_key,
            endpoint,
            notice_receiver: None,
        })
    }

    /// Connects to a Hyper server via Windows Named Pipe (async, Windows only).
    ///
    /// # Arguments
    ///
    /// * `pipe_path` - The full pipe path (e.g., `\\.\pipe\hyper-12345`)
    /// * `config` - Connection configuration
    ///
    /// # Errors
    ///
    /// Returns an error if the Named Pipe cannot be opened (e.g., pipe does not
    /// exist, all instances are busy after the retry window, or permission is
    /// denied) or if the authentication handshake fails.
    #[cfg(windows)]
    pub async fn connect_named_pipe(pipe_path: &str, config: &Config) -> Result<Self> {
        use std::time::{Duration, Instant};
        use tokio::net::windows::named_pipe::ClientOptions;

        info!(
            target: "hyperdb_api",
            pipe_path = %pipe_path,
            user = config.user().unwrap_or("(default)"),
            database = config.database().unwrap_or("(none)"),
            "connection-parameters-named-pipe"
        );

        // Retry on `ERROR_PIPE_BUSY` (231) — Windows named pipes have a finite
        // number of server-side instances and concurrent clients can hit the
        // cap. See the sync mirror in [`super::client::Client::connect_named_pipe`]
        // for the full rationale.
        const RETRY_INTERVAL: Duration = Duration::from_millis(20);
        const MAX_WAIT: Duration = Duration::from_secs(10);
        const ERROR_PIPE_BUSY: i32 = 231;

        let deadline = Instant::now() + MAX_WAIT;
        let client = loop {
            match ClientOptions::new().open(pipe_path) {
                Ok(c) => break c,
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
                {
                    tokio::time::sleep(RETRY_INTERVAL).await;
                }
                Err(e) => {
                    warn!(target: "hyperdb_api", pipe_path = %pipe_path, error = %e, "connection-failed");
                    return Err(Error::connection(format!(
                        "failed to connect to named pipe {pipe_path}: {e}"
                    )));
                }
            }
        };

        // Parse endpoint from pipe path
        let endpoint = ConnectionEndpoint::parse(&format!(
            "tab.pipe://{}",
            pipe_path.trim_start_matches(r"\\").replace('\\', "/")
        ))
        .unwrap_or_else(|_| {
            let parts: Vec<&str> = pipe_path
                .trim_start_matches(r"\\")
                .splitn(3, '\\')
                .collect();
            if parts.len() >= 3 {
                ConnectionEndpoint::named_pipe(parts[0], parts[2])
            } else {
                ConnectionEndpoint::named_pipe(".", pipe_path)
            }
        });

        let stream = AsyncStream::named_pipe(client);
        let mut connection = AsyncRawConnection::new(stream);

        // Perform startup with authentication
        let params = config.startup_params();
        let params_ref: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, *v)).collect();
        connection.startup(&params_ref, config.password()).await?;

        let process_id = connection.process_id();
        let secret_key = connection.secret_key();

        debug!(
            target: "hyperdb_api",
            process_id,
            "connection-established-named-pipe"
        );

        Ok(AsyncClient {
            connection: Arc::new(Mutex::new(connection)),
            process_id,
            secret_key,
            endpoint,
            notice_receiver: None,
        })
    }

    /// Connects to a Hyper server using a `ConnectionEndpoint` (async).
    ///
    /// This is a lower-level method that accepts a pre-parsed endpoint.
    ///
    /// # Errors
    ///
    /// Delegates to [`Self::connect`], [`Self::connect_unix`], or
    /// `Self::connect_named_pipe` depending on the endpoint variant,
    /// and propagates their errors unchanged.
    pub async fn connect_endpoint(endpoint: &ConnectionEndpoint, config: &Config) -> Result<Self> {
        match endpoint {
            ConnectionEndpoint::Tcp { host, port } => {
                let mut cfg = config.clone();
                cfg = cfg.with_host(host.clone()).with_port(*port);
                Self::connect(&cfg).await
            }
            #[cfg(unix)]
            ConnectionEndpoint::DomainSocket { directory, name } => {
                let socket_path = directory.join(name);
                Self::connect_unix(&socket_path, config).await
            }
            #[cfg(windows)]
            ConnectionEndpoint::NamedPipe { host, name } => {
                let pipe_path = format!(r"\\{host}\pipe\{name}");
                Self::connect_named_pipe(&pipe_path, config).await
            }
        }
    }

    /// Returns the connection endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &ConnectionEndpoint {
        &self.endpoint
    }

    /// Returns the server process ID for this connection.
    #[must_use]
    pub fn process_id(&self) -> i32 {
        self.process_id
    }

    /// Returns the secret key for cancel requests.
    #[must_use]
    pub fn secret_key(&self) -> i32 {
        self.secret_key
    }

    /// Cancels the currently executing query on this connection (async).
    ///
    /// This method opens a separate connection to send a cancel request.
    /// For TCP endpoints, it opens a new TCP connection.
    /// For Unix domain sockets, it connects to the same socket path.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if a fresh cancel-side socket
    ///   (TCP / UDS / named-pipe) cannot be opened to
    ///   [`Self::endpoint`].
    /// - Returns [`Error`] (I/O) if writing the cancel request fails.
    pub async fn cancel(&self) -> Result<()> {
        use crate::protocol::message::frontend;
        use bytes::BytesMut;
        use tokio::io::AsyncWriteExt;

        info!(
            target: "hyperdb_api",
            process_id = self.process_id,
            "query-cancel-request"
        );

        let endpoint_str = self.endpoint.to_string();

        match &self.endpoint {
            ConnectionEndpoint::Tcp { host, port } => {
                let addr = format!("{host}:{port}");
                let mut stream = TcpStream::connect(&addr).await.map_err(|e| {
                    warn!(
                        target: "hyperdb_api",
                        addr = %endpoint_str,
                        error = %e,
                        "query-cancel-connect-failed"
                    );
                    Error::connection(format!(
                        "failed to connect for cancel request to {endpoint_str}: {e}"
                    ))
                })?;
                // Cancel is a 16-byte fire-and-forget — disable Nagle so the
                // request hits the wire without waiting on a coalesce timer.
                stream.set_nodelay(true).ok();

                let mut buf = BytesMut::new();
                frontend::cancel_request(self.process_id, self.secret_key, &mut buf);

                stream.write_all(&buf).await.map_err(|e| {
                    warn!(
                        target: "hyperdb_api",
                        error = %e,
                        "query-cancel-send-failed"
                    );
                    Error::io(e)
                })?;
            }
            #[cfg(unix)]
            ConnectionEndpoint::DomainSocket { directory, name } => {
                let socket_path = directory.join(name);
                let mut stream = UnixStream::connect(&socket_path).await.map_err(|e| {
                    warn!(
                        target: "hyperdb_api",
                        addr = %endpoint_str,
                        error = %e,
                        "query-cancel-connect-failed"
                    );
                    Error::connection(format!(
                        "failed to connect for cancel request to {endpoint_str}: {e}"
                    ))
                })?;

                let mut buf = BytesMut::new();
                frontend::cancel_request(self.process_id, self.secret_key, &mut buf);

                stream.write_all(&buf).await.map_err(|e| {
                    warn!(
                        target: "hyperdb_api",
                        error = %e,
                        "query-cancel-send-failed"
                    );
                    Error::io(e)
                })?;
            }
            #[cfg(windows)]
            ConnectionEndpoint::NamedPipe { host, name } => {
                let pipe_path = format!(r"\\{host}\pipe\{name}");
                // Use sync file I/O for cancel (short-lived connection)
                let mut file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&pipe_path)
                    .map_err(|e| {
                        warn!(
                            target: "hyperdb_api",
                            addr = %endpoint_str,
                            error = %e,
                            "query-cancel-connect-failed"
                        );
                        Error::connection(format!(
                            "failed to connect for cancel request to {endpoint_str}: {e}"
                        ))
                    })?;

                let mut buf = BytesMut::new();
                frontend::cancel_request(self.process_id, self.secret_key, &mut buf);

                use std::io::Write;
                file.write_all(&buf).map_err(|e| {
                    warn!(
                        target: "hyperdb_api",
                        error = %e,
                        "query-cancel-send-failed"
                    );
                    Error::io(e)
                })?;

                file.flush().map_err(Error::io)?;
            }
        }

        debug!(target: "hyperdb_api", "query-cancel-sent");
        Ok(())
    }

    /// Executes a query and returns all result rows (async).
    ///
    /// # Errors
    ///
    /// Propagates any [`Error`] from the underlying connection's
    /// [`AsyncRawConnection::simple_query`] — [`Error`] (server) for
    /// server-side SQL errors, [`Error`] (I/O) / [`Error`] (closed) for
    /// transport failures, and [`Error`] (connection) if the connection
    /// is unhealthy. Row construction may also raise an [`Error`] when
    /// a `DataRow` cannot be decoded against its `RowDescription`.
    pub async fn query(&self, sql: &str) -> Result<Vec<Row>> {
        let mut conn = self.connection.lock().await;
        let messages = conn.simple_query(sql).await?;
        Self::process_query_messages(messages, self.notice_receiver.as_ref())
    }

    /// Executes a query with `HyperBinary` format for better performance (async).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::query`].
    pub async fn query_fast(&self, sql: &str) -> Result<Vec<StreamRow>> {
        let mut conn = self.connection.lock().await;
        let messages = conn.query_binary(sql).await?;
        Ok(Self::process_binary_messages(
            messages,
            self.notice_receiver.as_ref(),
        ))
    }

    /// Executes a query with `HyperBinary` format and returns a streaming
    /// result reader (async).
    ///
    /// This is the async mirror of
    /// [`Client::query_streaming`](super::client::Client::query_streaming).
    /// The returned [`AsyncQueryStream`] yields rows in chunks so callers
    /// can process arbitrarily large result sets with constant memory. The
    /// connection mutex is held for the duration of iteration; dropping the
    /// stream before completion issues a best-effort cancel and marks the
    /// connection desynchronized.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (I/O) if writing the Parse/Bind/Execute/Sync
    ///   sequence fails on the transport.
    pub async fn query_streaming(
        &self,
        sql: &str,
        chunk_size: usize,
    ) -> Result<AsyncQueryStream<'_>> {
        let mut conn = self.connection.lock().await;
        conn.start_query_binary(sql).await?;
        Ok(AsyncQueryStream::new(conn, self, chunk_size))
    }

    /// Sends a best-effort `CancelRequest` using *synchronous* I/O so it
    /// is usable from [`Drop`] impls (notably
    /// [`AsyncQueryStream::drop`](super::async_stream_query::AsyncQueryStream)).
    ///
    /// Cancellation opens a short-lived TCP / UDS / Named-Pipe connection,
    /// writes the cancel packet, and drops it — the server recognizes the
    /// (`process_id`, `secret_key`) tuple and signals the long-running query
    /// to abort. No response is expected.
    fn cancel_sync(&self) -> Result<()> {
        use crate::protocol::message::frontend;
        use bytes::BytesMut;
        use std::io::Write;

        info!(
            target: "hyperdb_api",
            process_id = self.process_id,
            "query-cancel-request"
        );

        let endpoint_str = self.endpoint.to_string();

        match &self.endpoint {
            ConnectionEndpoint::Tcp { host, port } => {
                let addr = format!("{host}:{port}");
                let mut stream = std::net::TcpStream::connect(&addr).map_err(|e| {
                    warn!(
                        target: "hyperdb_api",
                        addr = %endpoint_str,
                        error = %e,
                        "query-cancel-connect-failed"
                    );
                    Error::connection(format!(
                        "failed to connect for cancel request to {endpoint_str}: {e}"
                    ))
                })?;
                // Cancel is a 16-byte fire-and-forget — disable Nagle so the
                // request hits the wire without waiting on a coalesce timer.
                stream.set_nodelay(true).ok();

                let mut buf = BytesMut::with_capacity(16);
                frontend::cancel_request(self.process_id, self.secret_key, &mut buf);

                stream.write_all(&buf).map_err(Error::io)?;
                stream.flush().map_err(Error::io)?;
            }
            #[cfg(unix)]
            ConnectionEndpoint::DomainSocket { directory, name } => {
                let socket_path = directory.join(name);
                let mut stream =
                    std::os::unix::net::UnixStream::connect(&socket_path).map_err(|e| {
                        warn!(
                            target: "hyperdb_api",
                            addr = %endpoint_str,
                            error = %e,
                            "query-cancel-connect-failed"
                        );
                        Error::connection(format!(
                            "failed to connect for cancel request to {endpoint_str}: {e}"
                        ))
                    })?;

                let mut buf = BytesMut::with_capacity(16);
                frontend::cancel_request(self.process_id, self.secret_key, &mut buf);

                stream.write_all(&buf).map_err(Error::io)?;
                stream.flush().map_err(Error::io)?;
            }
            #[cfg(windows)]
            ConnectionEndpoint::NamedPipe { host, name } => {
                let pipe_path = format!(r"\\{host}\pipe\{name}");
                let mut file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&pipe_path)
                    .map_err(|e| {
                        warn!(
                            target: "hyperdb_api",
                            addr = %endpoint_str,
                            error = %e,
                            "query-cancel-connect-failed"
                        );
                        Error::connection(format!(
                            "failed to connect for cancel request to {endpoint_str}: {e}"
                        ))
                    })?;

                let mut buf = BytesMut::with_capacity(16);
                frontend::cancel_request(self.process_id, self.secret_key, &mut buf);

                file.write_all(&buf).map_err(Error::io)?;
                file.flush().map_err(Error::io)?;
            }
        }

        debug!(target: "hyperdb_api", "query-cancel-sent");
        Ok(())
    }

    /// Executes a command (INSERT/UPDATE/DELETE/DDL) and returns affected row count (async).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::query`] — server-side SQL errors,
    /// transport failures, and unhealthy-connection state all surface
    /// as [`Error`].
    pub async fn exec(&self, sql: &str) -> Result<u64> {
        let mut conn = self.connection.lock().await;
        let messages = conn.simple_query(sql).await?;
        Ok(Self::extract_row_count(&messages))
    }

    /// Returns a server parameter value by name.
    pub async fn parameter_status(&self, name: &str) -> Option<String> {
        let conn = self.connection.lock().await;
        conn.parameter_status(name)
            .map(std::string::ToString::to_string)
    }

    /// Sets the notice receiver callback.
    pub fn set_notice_receiver(&mut self, receiver: Option<Box<dyn Fn(Notice) + Send + Sync>>) {
        self.notice_receiver = receiver.map(Arc::from);
    }

    /// Closes the connection gracefully (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if writing the `Terminate` frame or
    /// flushing the async transport fails.
    pub async fn close(self) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.terminate().await
    }

    /// Executes a batch of statements separated by semicolons (async).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::query`].
    pub async fn batch_execute(&self, sql: &str) -> Result<()> {
        let mut conn = self.connection.lock().await;
        let _messages = conn.simple_query(sql).await?;
        Ok(())
    }

    /// Starts a COPY IN operation for bulk data insertion (async).
    ///
    /// # Errors
    ///
    /// Delegates to [`Self::copy_in_with_format`]; see that method
    /// for concrete failure modes.
    pub async fn copy_in(
        &self,
        table_name: &str,
        columns: &[&str],
    ) -> Result<AsyncCopyInWriter<'_>> {
        self.copy_in_with_format(table_name, columns, "HYPERBINARY")
            .await
    }

    /// Starts a COPY IN operation and returns an owned-handle writer
    /// whose lifetime is independent of this client. The writer holds an
    /// `Arc`-cloned reference to the underlying connection mutex, so it
    /// can be stored in structs that need a `'static`-lifetime writer —
    /// e.g. N-API classes that can't carry borrowed references across
    /// JS callbacks.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::copy_in_with_format`].
    pub async fn copy_in_arc_with_format(
        &self,
        table_name: &str,
        columns: &[&str],
        format: &str,
    ) -> Result<AsyncCopyInWriterOwned> {
        let mut conn = self.connection.lock().await;
        conn.start_copy_in_with_format(table_name, columns, format)
            .await?;
        drop(conn);
        Ok(AsyncCopyInWriterOwned::new(Arc::clone(&self.connection)))
    }

    /// Starts a COPY IN operation with a specified data format (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (server) if the server rejects the generated
    ///   `COPY ... FROM STDIN` statement.
    /// - Returns [`Error`] (I/O) on transport read/write failure.
    pub async fn copy_in_with_format(
        &self,
        table_name: &str,
        columns: &[&str],
        format: &str,
    ) -> Result<AsyncCopyInWriter<'_>> {
        let mut conn = self.connection.lock().await;
        conn.start_copy_in_with_format(table_name, columns, format)
            .await?;
        drop(conn);
        Ok(AsyncCopyInWriter::new(&self.connection))
    }

    /// Executes a COPY ... TO STDOUT query and returns all output data (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (server) when the server rejects the statement.
    /// - Returns [`Error`] (I/O) / [`Error`] (closed) on transport
    ///   read/write failure.
    pub async fn copy_out(&self, query: &str) -> Result<Vec<u8>> {
        let mut conn = self.connection.lock().await;
        conn.copy_out(query).await
    }

    /// Returns true if the connection is alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        // Try to acquire the lock - if we can, connection is alive
        self.connection.try_lock().is_ok()
    }

    /// Prepares a statement for execution (async).
    ///
    /// # Errors
    ///
    /// Delegates to [`Self::prepare_typed`]; see that method for the
    /// failure modes.
    pub async fn prepare(&self, query: &str) -> Result<AsyncPreparedStatement> {
        self.prepare_typed(query, &[]).await
    }

    /// Prepares a statement with explicit parameter types (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (server) if the server rejects the `Parse`
    ///   request (SQL syntax, unknown parameter OIDs, etc.).
    /// - Returns [`Error`] (I/O) on transport read/write failure.
    pub async fn prepare_typed(
        &self,
        query: &str,
        param_types: &[crate::types::Oid],
    ) -> Result<AsyncPreparedStatement> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let name = format!(
            "__hyper_async_stmt_{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut conn = self.connection.lock().await;
        let (params, columns) = conn.prepare(&name, query, param_types).await?;

        Ok(AsyncPreparedStatement {
            name,
            query: query.to_string(),
            param_types: params,
            columns,
            connection: Arc::downgrade(&self.connection),
            closed: false,
        })
    }

    /// Closes a prepared statement on the server (async).
    ///
    /// Prefer the RAII [`AsyncPreparedStatement::close`] method on the
    /// statement itself — it consumes the statement and prevents the
    /// auto-close Drop path from double-closing.
    ///
    /// # Errors
    ///
    /// Propagates any error from
    /// [`AsyncRawConnection::close_statement`] — unhealthy connection,
    /// server-side error during `Close`/`Sync`, or transport failure.
    pub async fn close_statement(&self, statement: &AsyncPreparedStatement) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.close_statement(&statement.name).await
    }

    /// Executes a prepared statement with parameters (async).
    ///
    /// # Errors
    ///
    /// Propagates any error from
    /// [`AsyncRawConnection::execute_prepared`] — unhealthy connection,
    /// parameter/type mismatch, server-side execution failure, or
    /// transport failure. Row construction may also raise an [`Error`]
    /// when a `DataRow` cannot be decoded.
    pub async fn execute_prepared<P: AsRef<[Option<Vec<u8>>]>>(
        &self,
        statement: &AsyncPreparedStatement,
        params: P,
    ) -> Result<Vec<Row>> {
        let params_ref: Vec<Option<&[u8]>> = params
            .as_ref()
            .iter()
            .map(|p| p.as_ref().map(std::vec::Vec::as_slice))
            .collect();

        let mut conn = self.connection.lock().await;
        conn.execute_prepared(&statement.name, &params_ref, statement.columns.len())
            .await
    }

    /// Executes a prepared statement that doesn't return rows (async).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::execute_prepared`] (excluding
    /// row-construction errors — this path never builds rows).
    pub async fn execute_prepared_no_result<P: AsRef<[Option<Vec<u8>>]>>(
        &self,
        statement: &AsyncPreparedStatement,
        params: P,
    ) -> Result<u64> {
        let params_ref: Vec<Option<&[u8]>> = params
            .as_ref()
            .iter()
            .map(|p| p.as_ref().map(std::vec::Vec::as_slice))
            .collect();

        let mut conn = self.connection.lock().await;
        conn.execute_prepared_no_result(&statement.name, &params_ref)
            .await
    }

    /// Executes a prepared statement with streaming results (async).
    ///
    /// Returns an [`AsyncPreparedQueryStream`](super::async_prepared_stream::AsyncPreparedQueryStream)
    /// that yields rows in chunks, keeping memory bounded regardless of
    /// result size. Async mirror of
    /// [`Client::execute_streaming`](crate::client::Client::execute_streaming).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (I/O) if writing the initial Bind/Execute/Sync
    ///   sequence fails on the transport.
    pub async fn execute_prepared_streaming<'a, P: AsRef<[Option<Vec<u8>>]>>(
        &'a self,
        statement: &AsyncPreparedStatement,
        params: P,
        chunk_size: usize,
    ) -> Result<super::async_prepared_stream::AsyncPreparedQueryStream<'a>> {
        let params_ref: Vec<Option<&[u8]>> = params
            .as_ref()
            .iter()
            .map(|p| p.as_ref().map(std::vec::Vec::as_slice))
            .collect();

        let mut conn = self.connection.lock().await;
        conn.start_execute_prepared(&statement.name, &params_ref, statement.columns.len())
            .await?;

        let columns = std::sync::Arc::new(statement.columns.clone());
        Ok(super::async_prepared_stream::AsyncPreparedQueryStream::new(
            conn, self, chunk_size, columns,
        ))
    }
}

impl Cancellable for AsyncClient {
    /// Fire-and-forget cancel via PG wire protocol `CancelRequest` on a
    /// fresh connection. Uses synchronous I/O so it is callable from
    /// `Drop` impls. Errors are logged and swallowed because cancellation
    /// is best-effort and callers cannot meaningfully recover.
    fn cancel(&self) {
        if let Err(e) = AsyncClient::cancel_sync(self) {
            warn!(
                target: "hyperdb_api_core::client",
                error = %e,
                process_id = self.process_id,
                "cancel request failed (best-effort, swallowed)",
            );
        }
    }
}

impl AsyncClient {
    fn process_query_messages(
        messages: Vec<Message>,
        notice_receiver: Option<&Arc<NoticeReceiver>>,
    ) -> Result<Vec<Row>> {
        use super::statement::{Column, ColumnFormat};

        let mut rows = Vec::new();
        let mut columns: Option<Arc<Vec<Column>>> = None;

        for msg in messages {
            match msg {
                Message::RowDescription(desc) => {
                    let mut cols = Vec::new();
                    for field in desc.fields().filter_map(std::result::Result::ok) {
                        cols.push(Column::new(
                            field.name().to_string(),
                            field.type_oid(),
                            field.type_modifier(),
                            ColumnFormat::from_code(field.format()),
                        ));
                    }
                    columns = Some(Arc::new(cols));
                }
                Message::DataRow(data) => {
                    if let Some(ref cols) = columns {
                        rows.push(Row::new(Arc::clone(cols), data)?);
                    }
                }
                Message::NoticeResponse(body) => {
                    if let Some(receiver) = notice_receiver {
                        let notice = Notice::from_response_body(&body);
                        receiver(notice);
                    }
                }
                _ => {}
            }
        }
        Ok(rows)
    }

    fn process_binary_messages(
        messages: Vec<Message>,
        notice_receiver: Option<&Arc<NoticeReceiver>>,
    ) -> Vec<StreamRow> {
        let mut rows = Vec::new();

        for msg in messages {
            match msg {
                Message::DataRow(data) => {
                    rows.push(StreamRow::new(data));
                }
                Message::NoticeResponse(body) => {
                    if let Some(receiver) = notice_receiver {
                        let notice = Notice::from_response_body(&body);
                        receiver(notice);
                    }
                }
                _ => {}
            }
        }
        rows
    }

    fn extract_row_count(messages: &[Message]) -> u64 {
        for msg in messages {
            if let Message::CommandComplete(body) = msg {
                if let Ok(tag) = body.tag() {
                    // Parse formats like "INSERT 0 5", "UPDATE 10", "DELETE 3"
                    let parts: Vec<&str> = tag.split_whitespace().collect();
                    if let Some(last) = parts.last() {
                        if let Ok(count) = last.parse() {
                            return count;
                        }
                    }
                }
            }
        }
        0
    }
}

/// An async prepared statement.
///
/// Represents a server-side prepared statement that can be executed
/// multiple times with different parameters. **Auto-closes on `Drop`**
/// via a best-effort `tokio::spawn` task — if no tokio runtime is
/// available at drop time we log a warning and flag the connection
/// desynchronized rather than silently leaking the server-side
/// statement slot.
///
/// For callers who need confirmed close with error propagation, use
/// [`AsyncPreparedStatement::close`] (explicit async close).
#[derive(Debug)]
pub struct AsyncPreparedStatement {
    /// Statement name on the server.
    pub(crate) name: String,
    /// Original SQL query string.
    query: String,
    /// Parameter type OIDs.
    param_types: Vec<crate::types::Oid>,
    /// Result column descriptions.
    pub(crate) columns: Vec<super::statement::Column>,
    /// Weak handle to the owning connection for the Drop path. `Weak`
    /// so that a lingering statement never keeps the connection alive
    /// past the `AsyncClient` it was prepared against.
    connection: std::sync::Weak<Mutex<AsyncRawConnection<AsyncStream>>>,
    /// Flipped by `close(self)` to suppress the Drop-path auto-close.
    closed: bool,
}

impl AsyncPreparedStatement {
    /// Returns the statement name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the original query.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Returns the parameter types.
    #[must_use]
    pub fn param_types(&self) -> &[crate::types::Oid] {
        &self.param_types
    }

    /// Returns the number of parameters.
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.param_types.len()
    }

    /// Returns the result column descriptions.
    #[must_use]
    pub fn columns(&self) -> &[super::statement::Column] {
        &self.columns
    }

    /// Returns the number of result columns.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Explicitly closes the prepared statement on the server (async).
    ///
    /// Consumes the statement — no further `execute_prepared` /
    /// `execute_prepared_streaming` calls are possible — and suppresses
    /// the Drop-path auto-close. Returns the `close_statement` result so
    /// callers can observe any transport errors.
    ///
    /// If you don't need error propagation, simply dropping the
    /// statement has the same effect (best-effort auto-close via
    /// `tokio::spawn`).
    ///
    /// # Errors
    ///
    /// Propagates any error from
    /// [`AsyncClient::close_statement`] — unhealthy connection,
    /// server-side error during `Close`/`Sync`, or transport failure.
    pub async fn close(mut self, client: &AsyncClient) -> Result<()> {
        self.closed = true;
        client.close_statement(&self).await
    }
}

impl Drop for AsyncPreparedStatement {
    fn drop(&mut self) {
        // Explicit close already ran — nothing to do.
        if self.closed {
            return;
        }

        // Best-effort close. Try to grab a tokio handle; if we're being
        // dropped outside a runtime (e.g. the runtime has already shut
        // down or the caller never had one), fall back to a warning and
        // flag the connection desynchronized so the next operation fails
        // loudly rather than racing with a lingering statement.
        let Some(conn) = self.connection.upgrade() else {
            // Connection has already been dropped — nothing to close.
            return;
        };

        let name = std::mem::take(&mut self.name);

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut c = conn.lock().await;
                if let Err(e) = c.close_statement(&name).await {
                    warn!(
                        target: "hyperdb_api_core::client",
                        statement = %name,
                        error = %e,
                        "AsyncPreparedStatement drop-close failed (best-effort, swallowed)"
                    );
                }
            });
        } else {
            // No runtime — can't do async I/O from here. Mark the
            // connection desynchronized so the next caller gets a
            // clear error instead of a latent leaked statement.
            if let Ok(mut c) = conn.try_lock() {
                c.mark_desynchronized();
            }
            warn!(
                target: "hyperdb_api_core::client",
                statement = %name,
                "AsyncPreparedStatement dropped outside of a tokio runtime; \
                 server-side statement slot leaked and connection marked \
                 desynchronized — call statement.close(&client) explicitly \
                 for deterministic cleanup"
            );
        }
    }
}

/// Async COPY IN writer for bulk data insertion.
///
/// # Drop Safety
///
/// If this writer is dropped without calling [`finish()`](Self::finish) or
/// [`cancel()`](Self::cancel), it will attempt a best-effort synchronous cancel
/// by queuing a `CopyFail` message in the connection's write buffer. The next
/// async operation on the connection will flush and drain the cancel response,
/// restoring the connection to a usable state.
#[derive(Debug)]
pub struct AsyncCopyInWriter<'a> {
    connection: &'a Mutex<AsyncRawConnection<AsyncStream>>,
    /// Set to `true` after `finish()` or `cancel()` consumes the writer.
    /// Checked in `Drop` to avoid queuing a spurious `CopyFail`.
    finished: bool,
}

/// Owned-handle variant of [`AsyncCopyInWriter`] that holds an
/// `Arc<Mutex<_>>` to the underlying connection instead of a borrow.
/// Used by callers that need a `'static`-lifetime writer — e.g. N-API
/// classes that can't carry borrowed references across JS callbacks.
///
/// Semantics are identical to [`AsyncCopyInWriter`]; the only
/// difference is lifetime.
#[derive(Debug)]
pub struct AsyncCopyInWriterOwned {
    connection: Arc<Mutex<AsyncRawConnection<AsyncStream>>>,
    finished: bool,
}

impl AsyncCopyInWriterOwned {
    /// Creates a new owned-handle COPY IN writer.
    pub(crate) fn new(connection: Arc<Mutex<AsyncRawConnection<AsyncStream>>>) -> Self {
        AsyncCopyInWriterOwned {
            connection,
            finished: false,
        }
    }

    /// Sends data to the server.
    ///
    /// # Errors
    ///
    /// Currently infallible — frame construction is pure. The `Result`
    /// return type is preserved for forward compatibility.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.send_copy_data(data)?;
        Ok(())
    }

    /// Flushes any buffered data to the server.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if flushing the async transport fails.
    pub async fn flush(&mut self) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.flush().await
    }

    /// Sends COPY data directly to the stream without internal buffering.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (protocol) if `data.len() + 4` exceeds
    ///   `u32::MAX`.
    /// - Returns [`Error`] (I/O) on transport write failure.
    pub async fn send_direct(&mut self, data: &[u8]) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.send_copy_data_direct(data).await
    }

    /// Flushes the TCP stream.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if flushing the async transport fails.
    pub async fn flush_stream(&mut self) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.flush_stream().await
    }

    /// Finishes the COPY operation and returns the number of rows inserted.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (server) if the server reports an `ErrorResponse`
    /// (e.g. constraint violation), or [`Error`] (I/O) / [`Error`] (closed)
    /// on transport failure.
    pub async fn finish(mut self) -> Result<u64> {
        self.finished = true;
        let mut conn = self.connection.lock().await;
        conn.finish_copy().await
    }

    /// Cancels the COPY operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) on transport write failure, or
    /// [`Error`] (closed) if the server drops the connection before
    /// acknowledging the cancel.
    pub async fn cancel(mut self, reason: &str) -> Result<()> {
        self.finished = true;
        let mut conn = self.connection.lock().await;
        conn.cancel_copy(reason).await
    }
}

impl Drop for AsyncCopyInWriterOwned {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Mirror AsyncCopyInWriter's Drop — queue a CopyFail via
        // try_lock; the next async operation on the connection will
        // flush and drain.
        if let Ok(mut conn) = self.connection.try_lock() {
            conn.queue_copy_fail("AsyncCopyInWriterOwned dropped without finish/cancel");
        }
    }
}

impl<'a> AsyncCopyInWriter<'a> {
    /// Creates a new COPY IN writer.
    pub(crate) fn new(connection: &'a Mutex<AsyncRawConnection<AsyncStream>>) -> Self {
        AsyncCopyInWriter {
            connection,
            finished: false,
        }
    }

    /// Sends data to the server.
    ///
    /// # Errors
    ///
    /// Currently infallible — frame construction is pure. The `Result`
    /// return type is preserved for forward compatibility.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.send_copy_data(data)?;
        Ok(())
    }

    /// Flushes any buffered data to the server.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if flushing the async transport fails.
    pub async fn flush(&mut self) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.flush().await
    }

    /// Sends COPY data directly to the stream without internal buffering.
    ///
    /// This writes data directly to the TCP stream, letting the kernel handle
    /// buffering. More efficient for streaming large amounts of data.
    /// Call `flush_stream()` periodically to ensure data is sent.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (protocol) if `data.len() + 4` exceeds
    ///   `u32::MAX`.
    /// - Returns [`Error`] (I/O) on transport write failure.
    pub async fn send_direct(&mut self, data: &[u8]) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.send_copy_data_direct(data).await
    }

    /// Flushes the TCP stream.
    ///
    /// Use with `send_direct()` to periodically ensure data reaches the server.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if flushing the async transport fails.
    pub async fn flush_stream(&mut self) -> Result<()> {
        let mut conn = self.connection.lock().await;
        conn.flush_stream().await
    }

    /// Finishes the COPY operation and returns the number of rows inserted.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (server) if the server reports an `ErrorResponse`,
    /// or [`Error`] (I/O) / [`Error`] (closed) on transport failure.
    pub async fn finish(mut self) -> Result<u64> {
        self.finished = true;
        let mut conn = self.connection.lock().await;
        conn.finish_copy().await
    }

    /// Cancels the COPY operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) on transport write failure, or
    /// [`Error`] (closed) if the server drops the connection before
    /// acknowledging the cancel.
    pub async fn cancel(mut self, reason: &str) -> Result<()> {
        self.finished = true;
        let mut conn = self.connection.lock().await;
        conn.cancel_copy(reason).await
    }
}

impl Drop for AsyncCopyInWriter<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Best-effort cancel: try to acquire the mutex synchronously and
        // queue a CopyFail message. We cannot do async I/O from Drop, so
        // the message is only written to the buffer — not flushed. The next
        // async operation will call drain_pending_copy_cancel() to flush it
        // and restore the connection to ReadyForQuery state.
        if let Ok(mut conn) = self.connection.try_lock() {
            conn.queue_copy_fail("COPY writer dropped without finish or cancel");
            warn!(
                target: "hyperdb_api_core::client",
                "AsyncCopyInWriter dropped without finish() or cancel(). \
                 Queued best-effort CopyFail — connection will self-heal on next operation."
            );
        } else {
            warn!(
                target: "hyperdb_api_core::client",
                "AsyncCopyInWriter dropped without finish() or cancel(), \
                 and the connection mutex was locked. The connection may be \
                 left in an unusable COPY-IN state."
            );
        }
    }
}
