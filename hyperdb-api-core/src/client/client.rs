// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! High-level synchronous client for Hyper database.
//!
//! This module provides [`Client`], the primary synchronous interface for
//! communicating with a Hyper server. It supports three query execution
//! modes, each using a different level of the `PostgreSQL` wire protocol:
//!
//! - **Simple Query** ([`Client::query`]) — Sends a single `Query` message
//!   and collects all `DataRow` messages into memory. Results are returned
//!   in text format. Best for small result sets or DDL/DML commands.
//!
//! - **Extended Query / `HyperBinary`** ([`Client::query_fast`]) — Uses the
//!   Extended Query protocol (`Parse` / `Bind` / `Execute`) with
//!   [`ColumnFormat::HyperBinary`](super::statement::ColumnFormat::HyperBinary)
//!   (format code 2) for `LittleEndian` binary results. Returns
//!   [`StreamRow`]s that compute field offsets
//!   on-demand, avoiding per-row allocation.
//!
//! - **Streaming** ([`Client::query_streaming`]) — Like `query_fast` but
//!   returns a [`QueryStream`] that yields rows in chunks, keeping memory
//!   usage constant regardless of result set size. The stream holds the
//!   connection lock for its lifetime; dropping it triggers a cancel request
//!   to stop the server from streaming the rest.
//!
//! # Bulk Insertion (COPY Protocol)
//!
//! [`Client::copy_in`] starts a `COPY ... FROM STDIN WITH (FORMAT HYPERBINARY)`
//! session and returns a [`CopyInWriter`] for streaming binary data. The
//! caller is responsible for encoding rows in the correct format (typically
//! done by the higher-level [`hyperdb_api::Inserter`](https://docs.rs/hyperdb-api)).
//! See also [`copy_in_with_format`](Client::copy_in_with_format) for
//! alternative formats (CSV, Arrow IPC) and [`copy_in_raw`](Client::copy_in_raw)
//! for fully custom COPY statements.

use std::net::TcpStream;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tracing::{debug, info, trace, warn};

/// Enable TCP keepalive on a connection socket so a half-open peer (laptop
/// sleep, network blip, a hyperd that vanished without a FIN) is detected in
/// ~90s instead of blocking a blocking `read()` until the OS default idle
/// timeout (7200s / 2h on macOS and Linux).
///
/// This matters most for long-lived idle connections — e.g. an MCP client
/// holding a connection to a resident daemon's hyperd across a laptop suspend.
/// Without keepalive, the next query on a silently-dead socket hangs for hours.
///
/// Tuning: 60s idle before the first probe, 10s between probes, 3 probes →
/// the peer is declared dead ~90s after it goes silent. Probe count is only
/// honored on platforms whose `socket2` build exposes `with_retries`; macOS
/// honors idle+interval. All calls are best-effort (`.ok()`): a kernel that
/// rejects a knob leaves the connection working at OS defaults.
fn apply_tcp_keepalive(sock: &socket2::SockRef<'_>) {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(10));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let keepalive = keepalive.with_retries(3);
    sock.set_tcp_keepalive(&keepalive).ok();
}

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use super::cancel::Cancellable;
use super::config::Config;
use super::connection::{parse_error_response, RawConnection};
use super::endpoint::ConnectionEndpoint;
use super::error::{Error, ErrorKind, Result};
use super::prepare;
use super::row::{Row, StreamRow};
use super::sync_stream::SyncStream;

use crate::protocol::message::Message;
use crate::types::Oid;

use super::notice::{Notice, NoticeReceiver};

/// A synchronous client for Hyper database.
///
/// The client handles connection management and query execution.
/// It is thread-safe and can be shared between threads using `Arc`.
///
/// # Thread Safety
///
/// The `Client` is thread-safe and can be shared between threads using `Arc<Client>`.
/// All methods use internal mutexes to synchronize access to the underlying connection.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api_core::client::{Client, Config};
///
/// # fn example() -> hyperdb_api_core::client::Result<()> {
/// let config = Config::new()
///     .with_host("localhost")
///     .with_port(7483)
///     .with_database("test.hyper");
///
/// let client = Client::connect(&config)?;
/// let rows = client.query("SELECT 1")?;
/// client.close()?;
/// # Ok(())
/// # }
/// ```
pub struct Client {
    /// The underlying connection, protected by a mutex for thread safety.
    connection: Arc<Mutex<RawConnection<SyncStream>>>,
    /// Backend process ID (for cancel requests).
    process_id: i32,
    /// Secret key for authenticating cancel requests.
    secret_key: i32,
    /// Connection endpoint for cancel requests and reconnection.
    endpoint: ConnectionEndpoint,
    /// Optional notice receiver callback for server notices/warnings.
    notice_receiver: Option<Arc<NoticeReceiver>>,
}

// Manual Debug implementation because NoticeReceiver doesn't implement Debug
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
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

impl Client {
    /// Connects to a Hyper server using the given configuration.
    ///
    /// Establishes a TCP connection, performs authentication, and initializes
    /// the client. Returns an error if the connection fails or authentication
    /// is rejected.
    ///
    /// # Arguments
    ///
    /// * `config` - Connection configuration (host, port, credentials, etc.)
    ///
    /// # Errors
    ///
    /// Returns `Error` if:
    /// - Connection to the server fails
    /// - Authentication fails
    /// - Protocol handshake fails
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::{Client, Config};
    /// # fn example() -> hyperdb_api_core::client::Result<()> {
    /// let config = Config::new()
    ///     .with_host("localhost")
    ///     .with_port(7483)
    ///     .with_user("myuser")
    ///     .with_password("mypass")
    ///     .with_database("test.hyper");
    ///
    /// let client = Client::connect(&config)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn connect(config: &Config) -> Result<Self> {
        // Log connection parameters (password is intentionally omitted for security)
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
        let tcp_stream = TcpStream::connect(&addr).map_err(|e| {
            warn!(target: "hyperdb_api", %addr, error = %e, "connection-failed");
            Error::connection(format!("failed to connect to {addr}: {e}"))
        })?;

        // Set TCP options for better performance.
        //
        // `TCP_NODELAY` disables Nagle so request bytes flush immediately —
        // needed for low-latency request/response shapes.
        //
        // `SO_RCVBUF` / `SO_SNDBUF` are bumped to 4 MiB. The Windows default
        // TCP buffers are ~64 KiB, which throttles loopback throughput
        // because hyperd blocks on `send()` once the kernel buffer fills up.
        // Linux auto-tunes much higher, so this primarily helps Windows
        // (and is a marginal win on macOS).
        //
        // Empirical knee on Windows i9-10980XE / TCP loopback (100M-row
        // sync full-scan, single connection):
        //
        // |  size | rows/sec |
        // |------:|---------:|
        // |  64 K |     2.89 |  (default)
        // |   1 M |     5.95 |
        // |   4 M |     6.90 |  <-- knee
        // |   8 M |     6.68 |  (insert workloads regress 18%)
        //
        // 4 MiB hits the throughput plateau without the memory-pressure
        // regression seen at 8 MiB. We use `.ok()` because the kernel may
        // clamp to a lower value or refuse the request entirely; either
        // way the connection still works at the default size.
        tcp_stream.set_nodelay(true).ok();
        let sock = socket2::SockRef::from(&tcp_stream);
        sock.set_recv_buffer_size(4 * 1024 * 1024).ok();
        sock.set_send_buffer_size(4 * 1024 * 1024).ok();
        apply_tcp_keepalive(&sock);

        let stream = SyncStream::tcp(tcp_stream);
        let mut connection = RawConnection::new(stream);

        // Perform startup with authentication
        let params = config.startup_params();
        let params_ref: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, *v)).collect();
        connection.startup(&params_ref, config.password())?;

        let process_id = connection.process_id();
        let secret_key = connection.secret_key();

        debug!(
            target: "hyperdb_api",
            process_id,
            "connection-established"
        );

        Ok(Client {
            connection: Arc::new(Mutex::new(connection)),
            process_id,
            secret_key,
            endpoint,
            notice_receiver: None,
        })
    }

    /// Connects to a Hyper server via Unix Domain Socket (Unix only).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::{Client, Config};
    /// # use std::path::Path;
    /// # fn example() -> hyperdb_api_core::client::Result<()> {
    /// let socket_path = Path::new("/tmp/hyper/.s.PGSQL.12345");
    /// let config = Config::new().with_database("test.hyper");
    /// let client = Client::connect_unix(socket_path, &config)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the Unix domain socket cannot
    ///   be connected.
    /// - Propagates any [`Error`] from the startup handshake
    ///   (authentication, protocol error, I/O error).
    #[cfg(unix)]
    pub fn connect_unix(socket_path: impl AsRef<std::path::Path>, config: &Config) -> Result<Self> {
        use std::path::Path;

        let path = socket_path.as_ref();
        info!(
            target: "hyperdb_api",
            socket_path = %path.display(),
            user = config.user().unwrap_or("(default)"),
            database = config.database().unwrap_or("(none)"),
            "connection-parameters-unix"
        );

        let unix_stream = UnixStream::connect(path).map_err(|e| {
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

        let stream = SyncStream::unix(unix_stream);
        let mut connection = RawConnection::new(stream);

        // Perform startup with authentication
        let params = config.startup_params();
        let params_ref: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, *v)).collect();
        connection.startup(&params_ref, config.password())?;

        let process_id = connection.process_id();
        let secret_key = connection.secret_key();

        debug!(
            target: "hyperdb_api",
            process_id,
            "connection-established-unix"
        );

        Ok(Client {
            connection: Arc::new(Mutex::new(connection)),
            process_id,
            secret_key,
            endpoint,
            notice_receiver: None,
        })
    }

    /// Connects to a Hyper server via Windows Named Pipe (Windows only).
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
    pub fn connect_named_pipe(pipe_path: &str, config: &Config) -> Result<Self> {
        use std::fs::OpenOptions;
        use std::time::{Duration, Instant};

        info!(
            target: "hyperdb_api",
            pipe_path = %pipe_path,
            user = config.user().unwrap_or("(default)"),
            database = config.database().unwrap_or("(none)"),
            "connection-parameters-named-pipe"
        );

        // Windows named pipes have a finite number of server-side instances
        // (`MaxInstances` on `CreateNamedPipe`). When all are busy, `CreateFile`
        // returns `ERROR_PIPE_BUSY` (231). The expected client behavior is to
        // wait briefly and retry — equivalent to `WaitNamedPipe` from Win32.
        // We poll with a short sleep up to a reasonable deadline so concurrent
        // clients don't spuriously fail when the pool is momentarily exhausted.
        const RETRY_INTERVAL: Duration = Duration::from_millis(20);
        const MAX_WAIT: Duration = Duration::from_secs(10);
        const ERROR_PIPE_BUSY: i32 = 231;

        let deadline = Instant::now() + MAX_WAIT;
        let file = loop {
            match OpenOptions::new().read(true).write(true).open(pipe_path) {
                Ok(f) => break f,
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
                {
                    std::thread::sleep(RETRY_INTERVAL);
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
            // Fallback: construct from pipe path directly
            // Expected format: \\<host>\pipe\<name>
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

        let stream = SyncStream::named_pipe(file);
        let mut connection = RawConnection::new(stream);

        // Perform startup with authentication
        let params = config.startup_params();
        let params_ref: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, *v)).collect();
        connection.startup(&params_ref, config.password())?;

        let process_id = connection.process_id();
        let secret_key = connection.secret_key();

        debug!(
            target: "hyperdb_api",
            process_id,
            "connection-established-named-pipe"
        );

        Ok(Client {
            connection: Arc::new(Mutex::new(connection)),
            process_id,
            secret_key,
            endpoint,
            notice_receiver: None,
        })
    }

    /// Connects to a Hyper server using a `ConnectionEndpoint`.
    ///
    /// This is a lower-level method that accepts a pre-parsed endpoint.
    ///
    /// # Errors
    ///
    /// Delegates to [`Client::connect`], [`Client::connect_unix`], or
    /// `Client::connect_named_pipe` depending on the endpoint variant,
    /// and propagates their errors unchanged.
    pub fn connect_endpoint(endpoint: &ConnectionEndpoint, config: &Config) -> Result<Self> {
        match endpoint {
            ConnectionEndpoint::Tcp { host, port } => {
                let mut cfg = config.clone();
                cfg = cfg.with_host(host.clone()).with_port(*port);
                Self::connect(&cfg)
            }
            #[cfg(unix)]
            ConnectionEndpoint::DomainSocket { directory, name } => {
                let socket_path = directory.join(name);
                Self::connect_unix(&socket_path, config)
            }
            #[cfg(windows)]
            ConnectionEndpoint::NamedPipe { host, name } => {
                let pipe_path = format!(r"\\{host}\pipe\{name}");
                Self::connect_named_pipe(&pipe_path, config)
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

    /// Cancels the currently executing query on this connection.
    ///
    /// This method is **thread-safe** and can be called from any thread while
    /// a query is running on another thread. It works by opening a separate
    /// TCP connection to the server and sending a cancel request.
    ///
    /// # How It Works
    ///
    /// 1. Opens a new TCP connection to the same server
    /// 2. Sends a cancel request containing the process ID and secret key
    /// 3. The server receives this and cancels the running query
    /// 4. The original query will fail with error code 57014 (`query_canceled`)
    ///
    /// # Thread Safety
    ///
    /// This method does NOT acquire the connection mutex, so it can be called
    /// while another thread is blocked waiting for query results.
    ///
    /// # Relation to the [`Cancellable`] trait
    ///
    /// This is the **fallible user-facing cancel API**: it returns a
    /// `Result<()>` so explicit callers can observe transport-level
    /// failures (network errors, socket issues) and react accordingly —
    /// e.g. record a metric, show "cancel failed" UX, or retry.
    ///
    /// For [`Drop`]-path and other internal cleanup contexts where error
    /// propagation is impossible, the separate
    /// [`impl Cancellable for Client`](super::cancel::Cancellable) wraps
    /// this method and swallows errors (logged via `tracing::warn!`).
    /// The two coexist by design — each serves a different consumer.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::thread;
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use hyperdb_api_core::client::{Client, Config};
    ///
    /// # fn example() -> hyperdb_api_core::client::Result<()> {
    /// # let config = Config::new().with_host("localhost").with_port(7483);
    /// let client = Arc::new(Client::connect(&config)?);
    /// let client_clone = Arc::clone(&client);
    ///
    /// // Start a long query in another thread
    /// let handle = thread::spawn(move || {
    ///     client_clone.query("SELECT pg_sleep(60)")
    /// });
    ///
    /// // Cancel from the main thread
    /// thread::sleep(Duration::from_millis(100));
    /// client.cancel()?;
    ///
    /// // The query thread will get a cancellation error
    /// let result = handle.join().unwrap();
    /// assert!(result.is_err());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the fresh cancel-side socket
    ///   cannot be opened (TCP / UDS / named-pipe, depending on
    ///   [`Self::endpoint`]).
    /// - Returns [`Error`] (I/O) if writing or flushing the cancel
    ///   request fails.
    pub fn cancel(&self) -> Result<()> {
        use crate::protocol::message::frontend;
        use bytes::BytesMut;
        use std::io::Write;

        info!(
            target: "hyperdb_api",
            process_id = self.process_id,
            "query-cancel-request"
        );

        let endpoint_str = self.endpoint.to_string();

        // Open a new connection specifically for the cancel request
        match &self.endpoint {
            ConnectionEndpoint::Tcp { host, port } => {
                let addr = format!("{host}:{port}");
                let mut stream = TcpStream::connect(&addr).map_err(|e| {
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

                // Build and send the cancel request
                let mut buf = BytesMut::with_capacity(16);
                frontend::cancel_request(self.process_id, self.secret_key, &mut buf);

                stream.write_all(&buf).map_err(|e| {
                    warn!(
                        target: "hyperdb_api",
                        error = %e,
                        "query-cancel-send-failed"
                    );
                    Error::io(e)
                })?;

                stream.flush().map_err(Error::io)?;
            }
            #[cfg(unix)]
            ConnectionEndpoint::DomainSocket { directory, name } => {
                let socket_path = directory.join(name);
                let mut stream = UnixStream::connect(&socket_path).map_err(|e| {
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

                // Build and send the cancel request
                let mut buf = BytesMut::with_capacity(16);
                frontend::cancel_request(self.process_id, self.secret_key, &mut buf);

                stream.write_all(&buf).map_err(|e| {
                    warn!(
                        target: "hyperdb_api",
                        error = %e,
                        "query-cancel-send-failed"
                    );
                    Error::io(e)
                })?;

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

                // Build and send the cancel request
                let mut buf = BytesMut::with_capacity(16);
                frontend::cancel_request(self.process_id, self.secret_key, &mut buf);

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

        debug!(
            target: "hyperdb_api",
            process_id = self.process_id,
            "query-cancel-sent"
        );

        Ok(())
    }

    /// Returns a server parameter value by name.
    ///
    /// Server parameters are sent by the server during connection startup.
    /// Common parameters include:
    /// - `server_version` - The server version string
    /// - `server_encoding` - The server's character encoding
    /// - `client_encoding` - The client's character encoding
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::{Client, Config};
    /// # fn example(client: &Client) {
    /// if let Some(version) = client.parameter_status("server_version") {
    ///     println!("Connected to Hyper version: {}", version);
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn parameter_status(&self, name: &str) -> Option<String> {
        let conn = self.connection.lock().ok()?;
        conn.parameter_status(name)
            .map(std::string::ToString::to_string)
    }

    /// Sets the notice receiver for this connection.
    ///
    /// Notice and warning messages generated by the server are not returned by
    /// query execution functions since they don't indicate failure. Instead,
    /// they are passed to a notice handling function.
    ///
    /// The default behavior is to log notices at the `warn` level.
    ///
    /// # Arguments
    ///
    /// * `receiver` - The callback function that will be called with each notice.
    ///   Pass `None` to restore default logging behavior.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::Client;
    /// # use std::sync::{Arc, Mutex};
    /// # fn example(client: &mut Client) {
    /// client.set_notice_receiver(Some(Box::new(|notice| {
    ///     println!("Server notice: {}", notice);
    /// })));
    ///
    /// // Or capture notices in a Vec
    /// let notices = Arc::new(Mutex::new(Vec::new()));
    /// let notices_clone = notices.clone();
    /// client.set_notice_receiver(Some(Box::new(move |notice| {
    ///     notices_clone.lock().unwrap().push(notice);
    /// })));
    /// # }
    /// ```
    pub fn set_notice_receiver(&mut self, receiver: Option<NoticeReceiver>) {
        self.notice_receiver = receiver.map(Arc::new);
    }

    /// Processes any notices in a list of messages, calling the notice receiver.
    ///
    /// This is called internally after receiving messages from the server.
    pub(crate) fn process_notices(&self, messages: &[Message]) {
        for msg in messages {
            if let Message::NoticeResponse(body) = msg {
                let notice = Notice::from_response_body(body);

                if let Some(ref receiver) = self.notice_receiver {
                    receiver(notice);
                } else {
                    // Default behavior: log at warn level
                    warn!(
                        target: "hyperdb_api",
                        severity = notice.severity().unwrap_or("NOTICE"),
                        code = notice.code().unwrap_or(""),
                        message = %notice.message(),
                        "server-notice"
                    );
                }
            }
        }
    }

    /// Acquires a lock on the connection.
    fn lock_connection(&self) -> Result<MutexGuard<'_, RawConnection<SyncStream>>> {
        self.connection
            .lock()
            .map_err(|_| Error::connection("connection mutex poisoned"))
    }

    /// Executes a simple query and returns the rows.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection mutex is
    ///   poisoned.
    /// - Returns [`Error`] (server) for any SQL error the server reports
    ///   (syntax error, constraint violation, type mismatch).
    /// - Returns [`Error`] (I/O) on wire-protocol I/O failure.
    /// - Propagates any [`Error`] from row construction (invalid row
    ///   description or data row bytes).
    pub fn query(&self, query: &str) -> Result<Vec<Row>> {
        let mut conn = self.lock_connection()?;
        let messages = conn.simple_query(query)?;
        drop(conn); // Release lock before processing notices

        self.process_notices(&messages);

        let mut rows = Vec::new();
        let mut columns = None;

        for msg in messages {
            match msg {
                crate::protocol::message::Message::RowDescription(desc) => {
                    // Extract column info including format from protocol
                    let mut cols = Vec::new();
                    for f in desc.fields().filter_map(|r| {
                        r.map_err(|e| trace!(target: "hyperdb_api_core::client", error = %e, "dropped error parsing row description field")).ok()
                    }) {
                        cols.push(super::statement::Column::new(
                            f.name().to_string(),
                            f.type_oid(),
                            f.type_modifier(),
                            super::statement::ColumnFormat::from_code(f.format()),
                        ));
                    }
                    columns = Some(Arc::new(cols));
                }
                crate::protocol::message::Message::DataRow(data) => {
                    if let Some(ref cols) = columns {
                        rows.push(Row::new(Arc::clone(cols), data)?);
                    }
                }
                _ => {}
            }
        }

        Ok(rows)
    }

    /// Executes a query using `HyperBinary` format for maximum performance.
    ///
    /// Returns `StreamRow`s which compute offsets on-demand without pre-allocation,
    /// making them faster for large result sets where each row is processed once.
    /// Uses the extended query protocol with `HyperBinary` format (format code 2)
    /// for direct binary access without text parsing overhead.
    ///
    /// # Errors
    ///
    /// Same as [`Self::query`]: connection-mutex poisoning, SQL errors
    /// from the server, and wire-protocol I/O failures all surface as
    /// [`Error`].
    pub fn query_fast(&self, query: &str) -> Result<Vec<StreamRow>> {
        let mut conn = self.lock_connection()?;
        let messages = conn.query_binary(query)?;
        drop(conn);

        self.process_notices(&messages);

        let mut rows = Vec::new();
        for msg in messages {
            if let crate::protocol::message::Message::DataRow(data) = msg {
                rows.push(StreamRow::new(data));
            }
        }
        Ok(rows)
    }

    /// Executes a query with streaming results for minimum memory usage.
    ///
    /// Combines `HyperBinary` format with incremental row fetching.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection mutex is
    ///   poisoned.
    /// - Returns [`Error`] (server) or [`Error`] (I/O) if the initial
    ///   `Parse`/`Bind`/`Execute` sequence for the streaming query
    ///   fails on the server or on the wire.
    pub fn query_streaming<'a>(
        &'a self,
        query: &str,
        chunk_size: usize,
    ) -> Result<QueryStream<'a>> {
        let mut conn = self.lock_connection()?;
        conn.start_query_binary(query)?;
        Ok(QueryStream {
            conn: Some(conn),
            // The owning client is the canceller: if the stream is dropped
            // before being fully drained, its `Drop` impl will call
            // `self.cancel()` (PG wire `CancelRequest` on a fresh
            // connection) to stop the server from streaming the rest.
            canceller: self,
            finished: false,
            chunk_size: chunk_size.max(1),
            schema: None,
            schema_read: false,
        })
    }

    /// Executes a SQL command that doesn't return rows (e.g., INSERT, UPDATE).
    ///
    /// # Errors
    ///
    /// Same error modes as [`Self::query`] — connection-mutex poisoning,
    /// server-side SQL errors, and wire-protocol I/O failures all
    /// surface as [`Error`].
    pub fn exec(&self, query: &str) -> Result<u64> {
        let mut conn = self.lock_connection()?;
        let messages = conn.simple_query(query)?;
        drop(conn); // Release lock before processing notices

        self.process_notices(&messages);

        let mut affected = 0u64;
        for msg in messages {
            if let crate::protocol::message::Message::CommandComplete(body) = msg {
                if let Ok(tag) = body.tag() {
                    // Parse affected row count from tag like "INSERT 0 1"
                    if let Some(count) = parse_affected_rows(tag) {
                        affected = count;
                    }
                }
            }
        }

        Ok(affected)
    }

    /// Executes a batch of statements separated by semicolons.
    ///
    /// # Errors
    ///
    /// Same error modes as [`Self::query`] — connection-mutex poisoning,
    /// server-side SQL errors, and wire-protocol I/O failures.
    pub fn batch_execute(&self, query: &str) -> Result<()> {
        let mut conn = self.lock_connection()?;
        let messages = conn.simple_query(query)?;
        drop(conn); // Release lock before processing notices

        self.process_notices(&messages);
        Ok(())
    }

    /// Prepares a statement for execution with the \[`params!`\] macro.
    ///
    /// Returns an [`prepare::OwnedPreparedStatement`] that automatically closes when dropped.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::{params, client::{Client, Config}};
    /// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
    /// let stmt = client.prepare("SELECT * FROM users WHERE id = $1")?;
    /// let rows = client.execute(&stmt, params![42_i32])?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Propagates any [`Error`] from [`prepare::prepare_owned`] —
    /// connection-mutex poisoning, server-side `Parse` failures (SQL
    /// syntax, type resolution), and wire-protocol I/O failures.
    pub fn prepare(&self, query: &str) -> Result<prepare::OwnedPreparedStatement> {
        prepare::prepare_owned(&self.connection, query, &[])
    }

    /// Prepares a statement with explicit parameter types.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::prepare`].
    pub fn prepare_typed(
        &self,
        query: &str,
        param_types: &[Oid],
    ) -> Result<prepare::OwnedPreparedStatement> {
        prepare::prepare_owned(&self.connection, query, param_types)
    }

    /// Executes a prepared statement with parameters.
    ///
    /// Use the \[`params!`\] macro for ergonomic parameter encoding:
    ///
    /// ```no_run
    /// # use hyperdb_api_core::{params, client::{Client, Config}};
    /// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
    /// let stmt = client.prepare("SELECT * FROM users WHERE id = $1 AND name = $2")?;
    /// let rows = client.execute(&stmt, params![42_i32, "Alice"])?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Propagates any [`Error`] from [`prepare::execute_prepared`] —
    /// parameter-count or type mismatch, server-side SQL errors, and
    /// wire-protocol I/O failures.
    pub fn execute<P: AsRef<[Option<Vec<u8>>]>>(
        &self,
        statement: &prepare::OwnedPreparedStatement,
        params: P,
    ) -> Result<Vec<Row>> {
        let params_ref: Vec<Option<&[u8]>> = params
            .as_ref()
            .iter()
            .map(|p| p.as_ref().map(std::vec::Vec::as_slice))
            .collect();
        prepare::execute_prepared(&self.connection, statement.statement(), &params_ref)
    }

    /// Executes a prepared statement that doesn't return rows.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::execute`].
    pub fn execute_no_result<P: AsRef<[Option<Vec<u8>>]>>(
        &self,
        statement: &prepare::OwnedPreparedStatement,
        params: P,
    ) -> Result<u64> {
        let params_ref: Vec<Option<&[u8]>> = params
            .as_ref()
            .iter()
            .map(|p| p.as_ref().map(std::vec::Vec::as_slice))
            .collect();
        prepare::execute_prepared_no_result(&self.connection, statement.statement(), &params_ref)
    }

    /// Executes a prepared statement with streaming results.
    ///
    /// Returns a [`PreparedQueryStream`](super::prepared_stream::PreparedQueryStream)
    /// that yields rows in chunks, keeping memory bounded regardless of
    /// result size. This is the prepared-statement analog of
    /// [`query_streaming`](Self::query_streaming).
    ///
    /// The connection mutex is held for the duration of iteration;
    /// dropping the stream before completion issues a best-effort cancel.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection mutex is
    ///   poisoned.
    /// - Returns [`Error`] (server) or [`Error`] (I/O) if the initial
    ///   `Bind`/`Execute` sequence for the prepared statement fails on
    ///   the server or on the wire.
    pub fn execute_streaming<'a, P: AsRef<[Option<Vec<u8>>]>>(
        &'a self,
        statement: &prepare::OwnedPreparedStatement,
        params: P,
        chunk_size: usize,
    ) -> Result<super::prepared_stream::PreparedQueryStream<'a>> {
        let params_ref: Vec<Option<&[u8]>> = params
            .as_ref()
            .iter()
            .map(|p| p.as_ref().map(std::vec::Vec::as_slice))
            .collect();

        let mut conn = self.lock_connection()?;
        conn.start_execute_prepared(statement.name(), &params_ref, statement.columns().len())?;

        let columns = std::sync::Arc::new(statement.columns().to_vec());
        Ok(super::prepared_stream::PreparedQueryStream::new(
            conn, self, chunk_size, columns,
        ))
    }

    /// Closes the connection.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection mutex is
    ///   poisoned.
    /// - Returns [`Error`] (I/O) if writing the `Terminate` message or
    ///   flushing the socket fails.
    pub fn close(self) -> Result<()> {
        let mut conn = self.lock_connection()?;
        conn.terminate()
    }

    /// Starts a COPY IN operation for bulk data insertion.
    ///
    /// Returns a `CopyInWriter` that can be used to send data in `HyperBinary` format.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::Client;
    /// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
    /// # let binary_data = &[];
    /// let mut writer = client.copy_in("\"my_table\"", &["col1", "col2"])?;
    /// writer.send(binary_data)?;
    /// let rows = writer.finish()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Delegates to [`Self::copy_in_with_format`]; see that method
    /// for the concrete failure modes.
    pub fn copy_in(&self, table_name: &str, columns: &[&str]) -> Result<CopyInWriter<'_>> {
        self.copy_in_with_format(table_name, columns, "HYPERBINARY")
    }

    /// Starts a COPY IN operation with a specified data format.
    ///
    /// Returns a `CopyInWriter` that can be used to send data in the specified format.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The target table name (should be properly quoted if needed)
    /// * `columns` - Column names to insert into
    /// * `format` - The data format string: "HYPERBINARY" or "ARROWSTREAM"
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::Client;
    /// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
    /// # let arrow_ipc_data = &[];
    /// // For Arrow IPC stream format
    /// let mut writer = client.copy_in_with_format("\"my_table\"", &["col1", "col2"], "ARROWSTREAM")?;
    /// writer.send(arrow_ipc_data)?;
    /// let rows = writer.finish()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection mutex is
    ///   poisoned.
    /// - Returns [`Error`] (server) if the server rejects the generated
    ///   `COPY ... FROM STDIN` statement (for example, missing table
    ///   or mismatched columns).
    /// - Returns [`Error`] (I/O) on wire-protocol I/O failure while
    ///   initiating the COPY.
    pub fn copy_in_with_format(
        &self,
        table_name: &str,
        columns: &[&str],
        format: &str,
    ) -> Result<CopyInWriter<'_>> {
        let mut conn = self.lock_connection()?;
        conn.start_copy_in_with_format(table_name, columns, format)?;
        Ok(CopyInWriter { connection: conn })
    }

    /// Starts a COPY IN operation from a raw SQL query string.
    ///
    /// The query must be a complete `COPY ... FROM STDIN ...` statement.
    /// This is useful for text-format imports (CSV, TSV) where you need
    /// full control over the COPY options.
    ///
    /// # Security
    ///
    /// The query is validated to start with `COPY` (case-insensitive) as a
    /// defense-in-depth measure. Callers are still responsible for proper
    /// escaping of table names and other identifiers within the query.
    /// Prefer [`copy_in()`](Self::copy_in) or
    /// [`copy_in_with_format()`](Self::copy_in_with_format) when possible,
    /// as they handle escaping automatically.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::Client;
    /// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
    /// let mut writer = client.copy_in_raw(
    ///     "COPY \"my_table\" FROM STDIN WITH (FORMAT csv, HEADER true)"
    /// )?;
    /// writer.send(b"1,Alice\n2,Bob\n")?;
    /// let rows = writer.finish()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorKind::Query`] if `query` (trimmed) does not
    ///   start with `COPY` (defense-in-depth check against non-COPY
    ///   statements).
    /// - Returns [`Error`] (connection) if the connection mutex is
    ///   poisoned.
    /// - Returns [`Error`] (server) or [`Error`] (I/O) if the server rejects
    ///   the COPY statement or the wire write fails.
    pub fn copy_in_raw(&self, query: &str) -> Result<CopyInWriter<'_>> {
        // Defense-in-depth: reject queries that don't look like COPY statements
        if !query.trim_start().to_ascii_uppercase().starts_with("COPY") {
            return Err(Error::new(
                ErrorKind::Query,
                "copy_in_raw() requires a COPY statement. \
                 The query must start with 'COPY'.",
            ));
        }
        let mut conn = self.lock_connection()?;
        conn.start_copy_in_raw(query)?;
        Ok(CopyInWriter { connection: conn })
    }

    /// Returns true if the connection is alive (no error has occurred).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.lock_connection().is_ok()
    }

    /// Executes a COPY ... TO STDOUT query and returns all output data.
    ///
    /// This is used for queries like:
    /// `COPY (SELECT ...) TO STDOUT WITH (format arrowstream)`
    ///
    /// # Arguments
    ///
    /// * `query` - The COPY TO STDOUT query to execute
    ///
    /// # Returns
    ///
    /// The raw bytes from all `CopyData` messages concatenated together.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::Client;
    /// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
    /// let arrow_data = client.copy_out(
    ///     "COPY (SELECT * FROM my_table) TO STDOUT WITH (format arrowstream)"
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection mutex is
    ///   poisoned.
    /// - Returns [`Error`] (server) if the server rejects the `COPY ... TO
    ///   STDOUT` statement.
    /// - Returns [`Error`] (I/O) if the wire read fails while collecting
    ///   COPY output.
    pub fn copy_out(&self, query: &str) -> Result<Vec<u8>> {
        let mut conn = self.lock_connection()?;
        conn.copy_out(query)
    }

    /// Executes a COPY ... TO STDOUT query and streams output to a writer.
    ///
    /// Unlike [`copy_out`](Self::copy_out) which collects all data into memory,
    /// this method streams each `CopyData` chunk directly to the provided writer,
    /// keeping memory usage constant regardless of result size.
    ///
    /// Returns the total number of bytes written.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api_core::client::Client;
    /// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
    /// let mut file = std::fs::File::create("output.csv")?;
    /// let bytes_written = client.copy_out_to_writer(
    ///     "COPY (SELECT * FROM my_table) TO STDOUT WITH (FORMAT csv, HEADER true)",
    ///     &mut file,
    /// )?;
    /// println!("Wrote {} bytes", bytes_written);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::copy_out`], plus [`Error`] (I/O)
    /// when the supplied `writer` returns an error while receiving
    /// COPY chunks.
    pub fn copy_out_to_writer(&self, query: &str, writer: &mut dyn std::io::Write) -> Result<u64> {
        let mut conn = self.lock_connection()?;
        conn.copy_out_to_writer(query, writer)
    }
}

impl Cancellable for Client {
    /// Fire-and-forget cancel via PG wire protocol `CancelRequest` on a
    /// fresh connection. Swallows errors (logged via `tracing::warn!`)
    /// because cancellation is a best-effort signal and callers cannot
    /// meaningfully recover from a failed cancel.
    fn cancel(&self) {
        if let Err(e) = Client::cancel(self) {
            warn!(
                target: "hyperdb_api_core::client",
                error = %e,
                process_id = self.process_id,
                "cancel request failed (best-effort, swallowed)",
            );
        }
    }
}

/// A writer for COPY IN operations.
///
/// This struct holds the connection lock while sending data to ensure
/// exclusive access during the COPY operation.
#[derive(Debug)]
pub struct CopyInWriter<'a> {
    connection: MutexGuard<'a, RawConnection<SyncStream>>,
}

impl CopyInWriter<'_> {
    /// Sends a chunk of COPY data.
    ///
    /// The data should be in `HyperBinary` format. For best performance,
    /// batch multiple rows into larger chunks (e.g., 1-16 MB).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if the wire write of the `CopyData` frame
    /// fails.
    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        self.connection.send_copy_data(data)
    }

    /// Flushes any buffered data to the server.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if flushing the internal write buffer to
    /// the transport fails.
    pub fn flush(&mut self) -> Result<()> {
        self.connection.flush()
    }

    /// Sends COPY data directly to the stream without internal buffering.
    ///
    /// This writes data directly to the TCP stream, letting the kernel handle
    /// buffering. More efficient for streaming large amounts of data.
    /// Call `flush_stream()` periodically to ensure data is sent.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if writing to the underlying stream fails.
    pub fn send_direct(&mut self, data: &[u8]) -> Result<()> {
        self.connection.send_copy_data_direct(data)
    }

    /// Flushes the TCP stream.
    ///
    /// Use with `send_direct()` to periodically ensure data reaches the server.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if flushing the underlying transport
    /// fails.
    pub fn flush_stream(&mut self) -> Result<()> {
        self.connection.flush_stream()
    }

    /// Reserves capacity in the write buffer to avoid reallocations.
    ///
    /// Call this before bulk operations to pre-allocate buffer space.
    pub fn reserve_buffer(&mut self, capacity: usize) {
        self.connection.reserve_write_buffer(capacity);
    }

    /// Finishes the COPY operation successfully.
    ///
    /// Returns the number of rows inserted.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (I/O) if writing `CopyDone` or flushing the
    ///   transport fails.
    /// - Returns [`Error`] (server) if the server reports a COPY-side
    ///   failure (constraint violation, type coercion error, etc.)
    ///   via an `ErrorResponse` after `CopyDone`.
    pub fn finish(mut self) -> Result<u64> {
        self.connection.finish_copy()
    }

    /// Cancels the COPY operation.
    ///
    /// All data sent so far will be discarded.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if writing the `CopyFail` frame or
    /// flushing the transport fails, or [`Error`] (server) if the server
    /// reports an unexpected status after the cancel.
    pub fn cancel(mut self, reason: &str) -> Result<()> {
        self.connection.cancel_copy(reason)
    }
}

/// Parses affected row count from a command tag.
fn parse_affected_rows(tag: &str) -> Option<u64> {
    let parts: Vec<&str> = tag.split_whitespace().collect();

    match parts.first()? {
        &"INSERT" => {
            // INSERT oid count
            parts.get(2)?.parse().ok()
        }
        &"UPDATE" | &"DELETE" | &"SELECT" | &"COPY" => {
            // UPDATE/DELETE/SELECT/COPY count
            parts.get(1)?.parse().ok()
        }
        _ => None,
    }
}

/// Streaming iterator for query results without materializing all rows.
///
/// Holding a `QueryStream` keeps the underlying [`RawConnection`] locked
/// via a `MutexGuard`. Dropping the stream before fully iterating triggers
/// a server-side cancel (see [`Drop`] below) so the connection is returned
/// to the pool cleanly.
pub struct QueryStream<'a> {
    conn: Option<MutexGuard<'a, RawConnection<SyncStream>>>,
    /// Best-effort cancel handle, used in [`Drop`] when the stream is
    /// abandoned before completion. For the current sync client this is
    /// the owning [`Client`] itself (which implements [`Cancellable`] via
    /// a PG wire `CancelRequest` on a fresh connection). When a gRPC
    /// equivalent lands it will plug in the same trait with a
    /// `cancel_query(query_id)` RPC implementation.
    canceller: &'a dyn Cancellable,
    finished: bool,
    chunk_size: usize,
    schema: Option<Vec<super::statement::Column>>,
    schema_read: bool,
}

impl std::fmt::Debug for QueryStream<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryStream")
            .field("finished", &self.finished)
            .field("chunk_size", &self.chunk_size)
            .field("schema_read", &self.schema_read)
            .finish_non_exhaustive()
    }
}

impl Drop for QueryStream<'_> {
    fn drop(&mut self) {
        // If the caller exhausted the stream, the connection is already at
        // `ReadyForQuery` — nothing to do.
        if self.finished {
            return;
        }

        // Otherwise: the server is still happily streaming rows (potentially
        // millions of them) for a query we no longer care about. Passively
        // draining would waste bandwidth and could block the destructor for
        // a very long time. Instead we send a transport-appropriate cancel
        // signal — on PG wire this is a `CancelRequest` packet on a fresh
        // connection; on gRPC (future) it will be a `cancel_query` RPC on
        // the shared channel. `Cancellable::cancel` is fire-and-forget and
        // cannot fail.
        self.canceller.cancel();

        // After cancel, the server stops producing new rows and emits
        // `ErrorResponse(QueryCanceled) + ReadyForQuery` promptly. We still
        // need to drain those trailing messages off the wire so the
        // connection returns to the pool cleanly. The budget here is small
        // because we only expect: (a) whatever rows the server had already
        // flushed before seeing the cancel, (b) `ErrorResponse`,
        // (c) `ReadyForQuery`. A well-behaved server reaches `ReadyForQuery`
        // within a handful of messages. If that budget is somehow exceeded
        // the bounded drain logs a warning and marks the connection
        // desynchronized — downstream users will surface the desync as a
        // transport-level failure and reconnect.
        const POST_CANCEL_DRAIN_CAP: usize = 1024;
        if let Some(ref mut conn) = self.conn {
            let _ok = conn.drain_until_ready_bounded(POST_CANCEL_DRAIN_CAP);
        }
    }
}

impl QueryStream<'_> {
    /// Returns the schema (column metadata) for the result set.
    #[must_use]
    pub fn schema(&self) -> Option<&[super::statement::Column]> {
        self.schema.as_deref()
    }

    /// Retrieves the next chunk of rows (up to `chunk_size`).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (I/O) if reading from the underlying transport
    ///   fails while awaiting the next protocol message.
    /// - Returns [`Error`] (server) when the server sends an `ErrorResponse`
    ///   during streaming (for example, a server-side execution failure
    ///   encountered partway through the result set).
    pub fn next_chunk(&mut self) -> Result<Option<Vec<StreamRow>>> {
        if self.finished {
            return Ok(None);
        }

        let Some(conn) = self.conn.as_mut() else {
            return Ok(None);
        };

        let mut rows = Vec::with_capacity(self.chunk_size);
        while rows.len() < self.chunk_size {
            let msg = conn.read_message()?;
            match msg {
                Message::RowDescription(desc) if !self.schema_read => {
                    let mut cols = Vec::new();
                    for f in desc.fields().filter_map(std::result::Result::ok) {
                        cols.push(super::statement::Column::new(
                            f.name().to_string(),
                            f.type_oid(),
                            f.type_modifier(),
                            super::statement::ColumnFormat::from_code(f.format()),
                        ));
                    }
                    self.schema = Some(cols);
                    self.schema_read = true;
                }
                Message::DataRow(data) => {
                    rows.push(StreamRow::new(data));
                    if rows.len() >= self.chunk_size {
                        return Ok(Some(rows));
                    }
                }
                Message::ReadyForQuery(_) => {
                    self.finished = true;
                    self.conn = None;
                    return if rows.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(rows))
                    };
                }
                Message::ErrorResponse(body) => {
                    // Mark the stream finished *before* touching the
                    // connection so the `Drop` impl's Cancellable-based
                    // cleanup path is trivially a no-op regardless of what
                    // happens next (normal return, `?`, panic in drain,
                    // future refactors that insert early returns, etc).
                    //
                    // `&mut self` exclusivity means `Drop` can't fire
                    // concurrently with this method, so this is purely a
                    // defensive-ordering improvement — but it also matches
                    // the `ReadyForQuery` arm above, which sets
                    // `finished = true` first for the same reason. Keeping
                    // both terminal arms in the same order makes the
                    // "terminal state is committed before cleanup" rule
                    // visible at a glance.
                    self.finished = true;
                    // Drain through `ReadyForQuery` before releasing the
                    // pooled connection so the next user sees a clean wire
                    // state. `consume_error` swallows any drain I/O errors
                    // via tracing::warn — the caller's original error is
                    // more informative than a transport hiccup during
                    // cleanup.
                    let err = match self.conn {
                        Some(ref mut c) => c.consume_error(&body),
                        None => parse_error_response(&body),
                    };
                    self.conn = None;
                    return Err(err);
                }
                _ => {}
            }
        }
        Ok(Some(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_affected_rows() {
        assert_eq!(parse_affected_rows("INSERT 0 5"), Some(5));
        assert_eq!(parse_affected_rows("UPDATE 10"), Some(10));
        assert_eq!(parse_affected_rows("DELETE 3"), Some(3));
        assert_eq!(parse_affected_rows("SELECT 100"), Some(100));
        assert_eq!(parse_affected_rows("CREATE TABLE"), None);
    }

    #[test]
    fn test_copy_in_raw_rejects_non_copy_query() {
        // We can't test with a real connection, but we can verify the prefix guard
        // works by checking the error message. Since Client::connect fails without
        // a server, we test the validation logic directly.
        let query = "SELECT * FROM users";
        assert!(
            !query.trim_start().to_ascii_uppercase().starts_with("COPY"),
            "Non-COPY query should not pass the COPY prefix check"
        );

        let copy_query = "COPY \"users\" FROM STDIN WITH (FORMAT csv)";
        assert!(
            copy_query
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("COPY"),
            "COPY query should pass the prefix check"
        );

        // Leading whitespace should be accepted
        let padded = "  COPY \"users\" FROM STDIN";
        assert!(padded.trim_start().to_ascii_uppercase().starts_with("COPY"));

        // Case-insensitive
        let lowercase = "copy \"users\" FROM STDIN";
        assert!(lowercase
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("COPY"));
    }
}
