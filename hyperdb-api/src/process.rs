// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Hyper server process management.
//!
//! This module provides [`HyperProcess`] for spawning and managing local hyperd server instances.
//!
//! # Callback Connection Architecture
//!
//! The `HyperProcess` uses a **callback connection** mechanism for reliable process lifecycle
//! management. This works as follows:
//!
//! 1. **Startup**: The client creates a TCP listener on an ephemeral port (the "callback proxy")
//!    and passes this address to hyperd via `--callback-connection`. Hyper connects back to this
//!    listener and sends its actual listen endpoint over this connection.
//!
//! 2. **Runtime**: The callback connection remains open for the lifetime of the `HyperProcess`.
//!    This connection acts as a "dead man's switch" - Hyper monitors it continuously.
//!
//! 3. **Graceful Shutdown**: When `HyperProcess` is dropped or explicitly shut down, the callback
//!    connection is closed. Hyper detects this and initiates graceful shutdown automatically.
//!
//! 4. **Crash Safety**: If the client process crashes or is killed, the OS automatically closes
//!    the TCP connection. Hyper detects this and shuts down gracefully, preventing orphan
//!    processes. This is the key advantage over signal-based shutdown.
//!
//! # Protocol Details
//!
//! The callback connection protocol is simple:
//! - Client listens on `127.0.0.1:<ephemeral_port>`
//! - Client starts hyperd with `--callback-connection=tab.tcp://127.0.0.1:<port>`
//! - Hyper connects to this address and sends: `[1 byte length][N bytes descriptor]`
//! - The descriptor is the actual listen endpoint (e.g., `tab.tcp://localhost:54321`)
//! - Connection stays open until shutdown is desired
//!
//! # Listen Modes
//!
//! `HyperProcess` supports different listen modes via [`ListenMode`]:
//!
//! - **`LibPq`** (default): `PostgreSQL` wire protocol for full read/write access
//! - **Grpc**: gRPC protocol for query-only Arrow-based access
//! - **Both**: Both protocols enabled (libpq for read/write, gRPC for Arrow queries)
//!
//! ```no_run
//! use hyperdb_api::{HyperProcess, ListenMode, Parameters, Result};
//!
//! fn main() -> Result<()> {
//!     // gRPC only
//!     let mut params = Parameters::new();
//!     params.set_listen_mode(ListenMode::Grpc { port: 0 });
//!     let hyper = HyperProcess::new(None, Some(&params))?;
//!     println!("gRPC endpoint: {}", hyper.grpc_endpoint().unwrap());
//!
//!     // Both libpq and gRPC
//!     let mut params = Parameters::new();
//!     params.set_listen_mode(ListenMode::Both { grpc_port: 7484 });
//!     let hyper = HyperProcess::new(None, Some(&params))?;
//!     println!("libpq endpoint: {}", hyper.endpoint().unwrap());
//!     println!("gRPC endpoint: {}", hyper.grpc_endpoint().unwrap());
//!     Ok(())
//! }
//! ```

use std::io::Read;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(any(unix, windows))]
use hyperdb_api_core::client::ConnectionEndpoint;

use tracing::info;

use crate::error::{Error, Result};

/// Specifies which protocols `HyperProcess` should listen on.
///
/// # Examples
///
/// ```
/// use hyperdb_api::{ListenMode, Parameters};
///
/// // LibPq only (default) - for full read/write access
/// let mut params = Parameters::new();
/// params.set_listen_mode(ListenMode::LibPq);
///
/// // gRPC only - for Arrow-based query access
/// let mut params = Parameters::new();
/// params.set_listen_mode(ListenMode::Grpc { port: 0 }); // auto-assign port
///
/// // Both protocols - libpq for writes, gRPC for Arrow queries
/// let mut params = Parameters::new();
/// params.set_listen_mode(ListenMode::Both { grpc_port: 7484 });
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListenMode {
    /// `PostgreSQL` wire protocol only (default).
    ///
    /// This is the traditional connection mode that supports all Hyper features
    /// including read and write operations.
    #[default]
    LibPq,

    /// gRPC protocol only.
    ///
    /// This mode is optimized for query-only workloads and returns results in
    /// Arrow IPC format. Note that gRPC mode does not support write operations.
    ///
    /// Set `port` to 0 to auto-assign an available port.
    Grpc {
        /// The port to listen on (0 for auto-assign).
        port: u16,
    },

    /// Both libpq and gRPC protocols.
    ///
    /// This mode enables full read/write access via libpq while also providing
    /// gRPC access for Arrow-based queries. The libpq port is auto-assigned,
    /// while the gRPC port is specified.
    ///
    /// Note: When using `Both` mode, the callback connection returns the libpq
    /// endpoint. Use `HyperProcess::grpc_endpoint()` to get the gRPC endpoint.
    Both {
        /// The gRPC port to listen on (cannot be 0 - must be a specific port).
        grpc_port: u16,
    },
}

/// A running Hyper server instance.
///
/// This struct manages the lifecycle of a local Hyper server process using a callback
/// connection for reliable shutdown. The server is automatically shut down when this
/// object is dropped.
///
/// # Callback Connection (Dead Man's Switch)
///
/// Unlike traditional process management that relies on signals, `HyperProcess` maintains
/// a TCP connection to the Hyper server. When this connection is closed (either explicitly
/// or because the client process exits), Hyper automatically shuts down gracefully.
///
/// This provides several benefits:
/// - **No orphan processes**: If your application crashes, Hyper shuts down automatically
/// - **Graceful shutdown**: Hyper can flush data and clean up properly
/// - **Cross-platform**: Works reliably on macOS, Linux, and Windows
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{HyperProcess, Result};
///
/// fn main() -> Result<()> {
///     // Start a Hyper server (auto-detect hyperd location)
///     let hyper = HyperProcess::new(None, None)?;
///
///     println!("Hyper server running at: {}", hyper.endpoint().unwrap());
///
///     // Server automatically shuts down when `hyper` goes out of scope
///     // via the callback connection mechanism
///     Ok(())
/// }
/// ```
#[must_use = "HyperProcess will shut down when dropped; store it to keep the server running"]
#[derive(Debug)]
pub struct HyperProcess {
    /// The child process handle.
    child: Option<Child>,
    /// The libpq endpoint descriptor (host:port or socket path), if libpq is enabled.
    endpoint: Option<String>,
    /// The parsed connection endpoint for libpq.
    connection_endpoint: Option<ConnectionEndpoint>,
    /// The gRPC endpoint (host:port), if gRPC is enabled.
    grpc_endpoint: Option<String>,
    /// Path to the hyperd executable.
    #[expect(
        dead_code,
        reason = "retained for diagnostics and future restart/respawn paths"
    )]
    hyperd_path: PathBuf,
    /// Whether shutdown has been initiated.
    shutdown_initiated: Arc<AtomicBool>,
    /// The callback connection to hyperd.
    /// Keeping this open maintains the "dead man's switch" - when dropped, Hyper shuts down.
    callback_connection: Option<TcpStream>,
    /// The listen mode this process was started with.
    listen_mode: ListenMode,
    /// The transport mode this process was started with.
    transport_mode: TransportMode,
    /// The socket directory for UDS connections (Unix only).
    /// This directory is automatically cleaned up on drop.
    #[cfg(unix)]
    socket_directory: Option<PathBuf>,
    /// The pipe name for Named Pipe connections (Windows only).
    #[cfg(windows)]
    pipe_name: Option<String>,
    /// The log directory where hyperd writes its log files.
    log_dir: Option<PathBuf>,
}

impl HyperProcess {
    /// Starts a new Hyper server instance.
    ///
    /// This method:
    /// 1. Creates a callback listener on an ephemeral port
    /// 2. Starts the hyperd process with the callback connection address
    /// 3. Waits for Hyper to connect back and provide its listen endpoint
    ///
    /// # Arguments
    ///
    /// * `hyper_path` - Optional path to the hyperd executable. If `None`, searches
    ///   in common locations (`HYPERD_PATH` env var, then known build output paths).
    /// * `parameters` - Optional parameters for the server.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The hyperd executable cannot be found
    /// - The callback listener cannot be created
    /// - The server fails to start
    /// - Hyper doesn't connect back within the timeout (30 seconds)
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{HyperProcess, Result};
    /// use std::path::Path;
    ///
    /// fn main() -> Result<()> {
    ///     // Auto-detect hyperd location
    ///     let hyper = HyperProcess::new(None, None)?;
    ///
    ///     // Or specify explicit path
    ///     let hyper2 = HyperProcess::new(
    ///         Some(Path::new("/path/to/hyperd")),
    ///         None,
    ///     )?;
    ///     Ok(())
    /// }
    /// ```
    pub fn new(hyper_path: Option<&Path>, parameters: Option<&Parameters>) -> Result<Self> {
        let hyperd_path = match hyper_path {
            Some(path) => path.to_path_buf(),
            None => Self::find_hyperd()?,
        };
        Self::start_server(&hyperd_path, parameters)
    }

    /// Resolves the hyperd executable from the `HYPERD_PATH` environment
    /// variable. The value can point at the executable directly, or at a
    /// directory containing it.
    ///
    /// If `HYPERD_PATH` is unset, returns an error instructing the caller
    /// to either set it or run the `hyperd-bootstrap` downloader to
    /// install a pinned release at `.hyperd/current/hyperd`.
    fn find_hyperd() -> Result<PathBuf> {
        #[cfg(windows)]
        const HYPERD_EXE: &str = "hyperd.exe";
        #[cfg(not(windows))]
        const HYPERD_EXE: &str = "hyperd";

        let Ok(path_str) = std::env::var("HYPERD_PATH") else {
            // Walk up from CWD looking for .hyperd/current/<exe> written by
            // `hyperd-bootstrap download`. This lets `node examples/foo.mjs`
            // run from any subdirectory of the repo without exporting HYPERD_PATH.
            if let Ok(cwd) = std::env::current_dir() {
                let mut dir = cwd.as_path();
                loop {
                    let candidate = dir.join(".hyperd").join("current").join(HYPERD_EXE);
                    if candidate.exists() {
                        return Ok(candidate);
                    }
                    match dir.parent() {
                        Some(parent) => dir = parent,
                        None => break,
                    }
                }
            }
            return Err(Error::config(
                "HYPERD_PATH is not set. Point it at a hyperd executable, \
                or run `make download-hyperd` (or `cargo run -p hyperd-bootstrap -- download`) \
                to install a pinned release at `.hyperd/current/hyperd`.",
            ));
        };

        let path = PathBuf::from(&path_str);
        if path.is_dir() {
            let with_exe = path.join(HYPERD_EXE);
            if with_exe.exists() {
                return Ok(with_exe);
            }
            #[cfg(windows)]
            {
                let without_exe = path.join("hyperd");
                if without_exe.exists() {
                    return Ok(without_exe);
                }
            }
            return Err(Error::config(format!(
                "HYPERD_PATH set to '{path_str}' but {HYPERD_EXE} not found in that directory"
            )));
        }
        if path.exists() {
            return Ok(path);
        }
        #[cfg(windows)]
        {
            let with_ext = PathBuf::from(format!("{path_str}.exe"));
            if with_ext.exists() {
                return Ok(with_ext);
            }
        }
        Err(Error::config(format!(
            "HYPERD_PATH set to '{}' but hyperd executable not found (checked: {})",
            path_str,
            path.display()
        )))
    }

    /// Starts the hyperd server process with callback connection.
    fn start_server(hyperd_path: &Path, parameters: Option<&Parameters>) -> Result<Self> {
        // Verify hyperd exists
        if !hyperd_path.exists() {
            return Err(Error::config(format!(
                "Hyper executable not found at: {}",
                hyperd_path.display()
            )));
        }

        info!(
            target: "hyperdb_api",
            path = %hyperd_path.display(),
            "hyperd-starting"
        );

        // Create callback listener on ephemeral port
        // This is the "dead man's switch" - when this connection is closed, Hyper shuts down
        let callback_listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| Error::internal(format!("Failed to create callback listener: {e}")))?;

        let callback_port = callback_listener
            .local_addr()
            .map_err(|e| Error::internal(format!("Failed to get callback port: {e}")))?
            .port();

        // Set a timeout for accepting the callback connection
        callback_listener.set_nonblocking(false).map_err(|e| {
            Error::internal(format!("Failed to set callback listener to blocking: {e}"))
        })?;

        // Check if user wants to disable default parameters
        let use_defaults = parameters.map_or(true, |p| !p.contains_key(NO_DEFAULT_PARAMETERS));

        // Get the listen mode
        let listen_mode = parameters.and_then(|p| p.listen_mode).unwrap_or_default();

        // Get transport mode (default to TCP until UDS performance is validated)
        // See IPC_IMPLEMENTATION.md for details
        #[cfg(unix)]
        let transport_mode = parameters
            .and_then(|p| p.transport_mode)
            .unwrap_or(TransportMode::Tcp);
        #[cfg(windows)]
        let transport_mode = parameters
            .and_then(|p| p.transport_mode)
            .unwrap_or(TransportMode::Tcp);
        #[cfg(not(any(unix, windows)))]
        let transport_mode = TransportMode::Tcp;

        // Create socket directory for UDS if needed (Unix only)
        #[cfg(unix)]
        let socket_directory: Option<PathBuf> = if transport_mode == TransportMode::Ipc {
            // Use custom directory if provided, otherwise create temp directory
            let dir = if let Some(custom_dir) =
                parameters.and_then(|p| p.domain_socket_directory.as_ref())
            {
                custom_dir.clone()
            } else {
                // Create a temp directory for the socket
                let temp_dir = std::env::temp_dir().join(format!("hyper-{}", std::process::id()));
                std::fs::create_dir_all(&temp_dir).map_err(|e| {
                    Error::internal(format!("Failed to create socket directory: {e}"))
                })?;
                temp_dir
            };
            Some(dir)
        } else {
            None
        };

        // On non-Unix platforms there is no UDS socket directory; the variable
        // is only referenced inside `#[cfg(unix)]` blocks so we do not need a
        // placeholder binding here.

        // Create pipe name for Named Pipes if needed (Windows only)
        #[cfg(windows)]
        let pipe_name: Option<String> = if transport_mode == TransportMode::Ipc {
            Some(format!("hyper-{}", std::process::id()))
        } else {
            None
        };

        // Build command arguments
        let mut cmd = Command::new(hyperd_path);

        // The "run" subcommand starts the server
        cmd.arg("run");

        // Callback connection - Hyper will connect to this and send its endpoint
        // When this connection is closed, Hyper will shut down gracefully
        cmd.arg("--callback-connection")
            .arg(format!("tab.tcp://127.0.0.1:{callback_port}"));

        // Configure listen connection based on mode and transport
        // Connection string formats:
        // - tab.tcp://host:port - libpq over TCP
        // - tab.domain://<dir>/domain/<name> - libpq over Unix Domain Socket
        // - tcp.grpc://host:port - gRPC
        #[cfg(unix)]
        let listen_connection = if transport_mode == TransportMode::Ipc {
            let socket_dir = socket_directory.as_ref().unwrap();
            match listen_mode {
                ListenMode::LibPq => format!("tab.domain://{}/domain/hyper", socket_dir.display()),
                ListenMode::Grpc { port } => format!("tcp.grpc://127.0.0.1:{port}"),
                ListenMode::Both { grpc_port } => {
                    format!(
                        "tab.domain://{}/domain/hyper,tcp.grpc://127.0.0.1:{}",
                        socket_dir.display(),
                        grpc_port
                    )
                }
            }
        } else {
            match listen_mode {
                ListenMode::LibPq => "tab.tcp://localhost:0".to_string(),
                ListenMode::Grpc { port } => format!("tcp.grpc://127.0.0.1:{port}"),
                ListenMode::Both { grpc_port } => {
                    format!("tab.tcp://localhost:0,tcp.grpc://127.0.0.1:{grpc_port}")
                }
            }
        };

        #[cfg(windows)]
        let listen_connection = if transport_mode == TransportMode::Ipc {
            let pname = pipe_name.as_ref().unwrap();
            match listen_mode {
                ListenMode::LibPq => format!("tab.pipe://./pipe/{pname}"),
                ListenMode::Grpc { port } => format!("tcp.grpc://127.0.0.1:{port}"),
                ListenMode::Both { grpc_port } => {
                    format!("tab.pipe://./pipe/{pname},tcp.grpc://127.0.0.1:{grpc_port}")
                }
            }
        } else {
            match listen_mode {
                ListenMode::LibPq => "tab.tcp://localhost:0".to_string(),
                ListenMode::Grpc { port } => format!("tcp.grpc://127.0.0.1:{port}"),
                ListenMode::Both { grpc_port } => {
                    format!("tab.tcp://localhost:0,tcp.grpc://127.0.0.1:{grpc_port}")
                }
            }
        };

        #[cfg(not(any(unix, windows)))]
        let listen_connection = match listen_mode {
            ListenMode::LibPq => "tab.tcp://localhost:0".to_string(),
            ListenMode::Grpc { port } => format!("tcp.grpc://127.0.0.1:{}", port),
            ListenMode::Both { grpc_port } => {
                format!("tab.tcp://localhost:0,tcp.grpc://127.0.0.1:{}", grpc_port)
            }
        };

        cmd.arg("--listen-connection").arg(&listen_connection);

        // Helper to check if a parameter is already set by the user
        let user_has_param =
            |key: &str| -> bool { parameters.is_some_and(|p| p.contains_key(key)) };

        // Apply default instance parameters (matching C++ HyperProcess behavior)
        // These can be overridden by user parameters or disabled entirely with NO_DEFAULT_PARAMETERS
        if use_defaults {
            // Initial user for the Hyper instance
            if !user_has_param("init_user") {
                cmd.arg("--init-user=tableau_internal_user");
            }

            // Enable gRPC threads if gRPC mode is enabled
            // Required for gRPC to function - without this, Hyper will fail to start with:
            // "gRPC threads are required for running gRPC services"
            // Using 4 threads as a reasonable default (can be overridden by user)
            if matches!(
                listen_mode,
                ListenMode::Grpc { .. } | ListenMode::Both { .. }
            ) && !user_has_param("grpc_threads")
            {
                cmd.arg("--grpc-threads=4");
            }

            // Enable gRPC result persistence if gRPC mode is enabled
            // This is required for ADAPTIVE and ASYNC transfer modes
            if matches!(
                listen_mode,
                ListenMode::Grpc { .. } | ListenMode::Both { .. }
            ) && !user_has_param("grpc_persist_results")
            {
                cmd.arg("--grpc-persist-results=true");
            }

            // Default language setting
            if !user_has_param("language") {
                cmd.arg("--language=en_US");
            }

            // Log configuration: file-based JSON logging
            if !user_has_param("log_config") {
                cmd.arg(format!("--log-config={DEFAULT_LOG_CONFIG}"));
            }

            // Date style for date parsing (Month-Day-Year)
            if !user_has_param("date_style") {
                cmd.arg("--date-style=MDY");
            }

            // Enforce strict date_style (day/month/year ordering must match exactly)
            if !user_has_param("date_style_lenient") {
                cmd.arg("--date-style-lenient=false");
            }

            // Set default log directory to current directory
            if !user_has_param("log_dir") {
                if let Ok(cwd) = std::env::current_dir() {
                    cmd.arg(format!("--log-dir={}", cwd.display()));
                }
            }

            // Disable password requirement for local development
            if !user_has_param("no_password") {
                cmd.arg("--no-password");
            }

            // Skip license check for local development
            if !user_has_param("skip_license") {
                cmd.arg("--skip-license");
            }

            // Default new .hyper databases to file format version 3, which
            // adds support for 128-bit NUMERICs (required to ingest parquet
            // files whose decimal columns are stored as DECIMAL128).
            // File format 3 has shipped since Hyper 2022.4.0.
            if !user_has_param("default_database_version") {
                cmd.arg("--default-database-version=3");
            }
        }

        // Add custom parameters from user
        if let Some(params) = parameters {
            for (key, value) in params.iter() {
                // Skip internal/special parameters
                if key == "callback_connection"
                    || key == "listen_connection"
                    || key == NO_DEFAULT_PARAMETERS
                {
                    continue;
                }

                // Convert underscores to dashes for command-line arguments
                let cli_key = key.replace('_', "-");

                if value.is_empty() {
                    cmd.arg(format!("--{cli_key}"));
                } else {
                    cmd.arg(format!("--{cli_key}={value}"));
                }
            }
        }

        // Resolve the effective log directory for later access via log_dir()
        let resolved_log_dir =
            if let Some(user_dir) = parameters.and_then(|p| p.get("log_dir")).map(PathBuf::from) {
                Some(user_dir)
            } else if use_defaults {
                std::env::current_dir().ok()
            } else {
                None
            };

        // Redirect stdout/stderr to null - we get the endpoint via callback connection
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        // On Unix, start hyperd in its own process group so it doesn't receive
        // Ctrl-C (SIGINT) signals meant for the parent process. This allows the
        // parent to handle Ctrl-C gracefully and properly shut down hyperd via
        // the callback connection mechanism.
        #[cfg(unix)]
        cmd.process_group(0);

        // On Windows, prevent a console window from flashing when spawning hyperd.
        // CREATE_NO_WINDOW (0x08000000) suppresses the creation of a visible console.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        // Start the process
        let child = cmd.spawn().map_err(|e| {
            Error::internal(format!(
                "Failed to start Hyper server at {}: {}",
                hyperd_path.display(),
                e
            ))
        })?;

        // Wait for Hyper to connect back to our callback listener
        let (callback_connection, callback_endpoint) = Self::wait_for_callback(&callback_listener)?;

        // Determine the endpoints based on listen mode and callback response
        let (endpoint, grpc_endpoint) = match listen_mode {
            ListenMode::LibPq => {
                // Callback returns libpq endpoint
                (Some(callback_endpoint), None)
            }
            ListenMode::Grpc { port } => {
                // Callback returns gRPC endpoint (with resolved port if auto-assigned)
                let grpc_ep = if port == 0 {
                    // The callback returns the actual gRPC endpoint with resolved port
                    callback_endpoint
                } else {
                    format!("127.0.0.1:{port}")
                };
                (None, Some(grpc_ep))
            }
            ListenMode::Both { grpc_port } => {
                // Callback returns libpq endpoint, gRPC uses specified port
                (
                    Some(callback_endpoint),
                    Some(format!("127.0.0.1:{grpc_port}")),
                )
            }
        };

        // Parse connection endpoint if we have a libpq endpoint
        let connection_endpoint = endpoint.as_ref().map(|ep| {
            #[cfg(unix)]
            {
                // Check if it's a UDS path (contains path separator but no colon with port)
                if ep.starts_with('/') || socket_directory.is_some() {
                    // UDS endpoint - construct from socket directory
                    if let Some(ref dir) = socket_directory {
                        return ConnectionEndpoint::domain_socket(dir, "hyper");
                    }
                    // Parse as path
                    let path = std::path::Path::new(ep);
                    let dir = path.parent().unwrap_or(std::path::Path::new("/"));
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("hyper");
                    return ConnectionEndpoint::domain_socket(dir, name);
                }
            }
            #[cfg(windows)]
            {
                // Check if it's a Named Pipe endpoint
                if pipe_name.is_some() {
                    if let Some(ref pname) = pipe_name {
                        return ConnectionEndpoint::named_pipe(".", pname);
                    }
                }
            }
            // TCP endpoint (host:port format)
            let parts: Vec<&str> = ep.split(':').collect();
            if parts.len() == 2 {
                if let Ok(port) = parts[1].parse::<u16>() {
                    return ConnectionEndpoint::tcp(parts[0], port);
                }
            }
            ConnectionEndpoint::tcp("localhost", 7483) // fallback
        });

        Ok(HyperProcess {
            child: Some(child),
            endpoint,
            connection_endpoint,
            grpc_endpoint,
            hyperd_path: hyperd_path.to_path_buf(),
            shutdown_initiated: Arc::new(AtomicBool::new(false)),
            callback_connection: Some(callback_connection),
            listen_mode,
            transport_mode,
            log_dir: resolved_log_dir,
            #[cfg(unix)]
            socket_directory,
            #[cfg(windows)]
            pipe_name,
        })
    }

    /// Waits for Hyper to connect to our callback listener and send its endpoint.
    ///
    /// Protocol:
    /// - Hyper connects to the callback listener
    /// - Hyper sends: [1 byte length][N bytes connection descriptor string]
    /// - Connection descriptor format: "tab.tcp://host:port"
    fn wait_for_callback(listener: &TcpListener) -> Result<(TcpStream, String)> {
        // Set a timeout for accepting connections
        listener.set_nonblocking(true).ok();

        let timeout = Duration::from_secs(60);
        let start = std::time::Instant::now();

        // Poll for incoming connection with timeout
        let mut stream = loop {
            if start.elapsed() > timeout {
                return Err(Error::internal(
                    "Timeout waiting for Hyper to connect to callback listener. \
                    Hyper may have failed to start - check hyperd logs for details.",
                ));
            }

            match listener.accept() {
                Ok((stream, _addr)) => break stream,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    return Err(Error::internal(format!(
                        "Failed to accept callback connection: {e}"
                    )));
                }
            }
        };

        // Set stream back to blocking for reading
        stream.set_nonblocking(false).map_err(|e| {
            Error::internal(format!("Failed to set callback stream to blocking: {e}"))
        })?;

        // Set read timeout
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();

        // Read the endpoint descriptor from Hyper
        // Protocol: [1 byte length][N bytes descriptor string]
        let mut len_buf = [0u8; 1];
        stream.read_exact(&mut len_buf).map_err(|e| {
            Error::internal(format!("Failed to read endpoint length from Hyper: {e}"))
        })?;

        let len = len_buf[0] as usize;
        if len == 0 {
            return Err(Error::internal("Hyper sent empty endpoint descriptor"));
        }

        let mut descriptor_buf = vec![0u8; len];
        stream.read_exact(&mut descriptor_buf).map_err(|e| {
            Error::internal(format!(
                "Failed to read endpoint descriptor from Hyper: {e}"
            ))
        })?;

        let descriptor = String::from_utf8(descriptor_buf)
            .map_err(|e| Error::internal(format!("Invalid UTF-8 in endpoint descriptor: {e}")))?;

        // Trim null bytes and whitespace that Hyper may include
        let descriptor = descriptor.trim_matches(|c: char| c == '\0' || c.is_whitespace());

        // Parse the connection descriptor (format: "tab.tcp://host:port")
        let endpoint = Self::parse_connection_descriptor(descriptor)?;

        // Clear read timeout for the connection we'll keep open
        stream.set_read_timeout(None).ok();

        info!(
            target: "hyperdb_api",
            %endpoint,
            "hyperd-started"
        );

        Ok((stream, endpoint))
    }

    /// Parses a connection descriptor to extract host:port, socket path, or pipe path.
    ///
    /// Input formats:
    /// - "tab.tcp://host:port" → "host:port"
    /// - "tab.domain://<dir>/domain/<name>" → "<dir>/domain/<name>" (socket path)
    /// - "tab.pipe://<host>/pipe/<name>" → "<host>/pipe/<name>" (named pipe)
    /// - "tcp.grpc://host:port" → "host:port"
    fn parse_connection_descriptor(descriptor: &str) -> Result<String> {
        // Handle domain socket format
        if let Some(rest) = descriptor.strip_prefix("tab.domain://") {
            // Return the full path for UDS
            if let Some(idx) = rest.find("/domain/") {
                let dir = &rest[..idx];
                let name = &rest[idx + 8..]; // "/domain/".len() == 8
                let socket_path = format!("{dir}/domain/{name}");
                return Ok(socket_path);
            }
            return Ok(rest.to_string());
        }

        // Handle named pipe format
        if let Some(rest) = descriptor.strip_prefix("tab.pipe://") {
            // Format: tab.pipe://<host>/pipe/<name>
            // Return as pipe path: \\<host>\pipe\<name>
            if let Some(idx) = rest.find("/pipe/") {
                let host = &rest[..idx];
                let name = &rest[idx + 6..]; // "/pipe/".len() == 6
                let pipe_path = format!(r"\\{host}\pipe\{name}");
                return Ok(pipe_path);
            }
            return Ok(rest.to_string());
        }

        // Handle TCP prefixes
        let without_prefix = descriptor
            .strip_prefix("tab.tcp://")
            .or_else(|| descriptor.strip_prefix("tcp.grpc://"))
            .or_else(|| descriptor.strip_prefix("tcp.grpctls://"))
            .or_else(|| descriptor.strip_prefix("tcp://"))
            .unwrap_or(descriptor);

        // Validate it looks like host:port
        if without_prefix.contains(':') && !without_prefix.is_empty() {
            Ok(without_prefix.to_string())
        } else {
            Err(Error::internal(format!(
                "Invalid connection descriptor format: '{descriptor}'. Expected '<scheme>://host:port' or 'tab.domain://<dir>/domain/<name>'"
            )))
        }
    }

    /// Returns the libpq endpoint for connecting to this instance.
    ///
    /// The endpoint is in the format "host:port" (e.g., "localhost:54321").
    ///
    /// Returns `None` if the process was started in gRPC-only mode.
    /// Use [`grpc_endpoint`](Self::grpc_endpoint) for gRPC connections.
    #[must_use]
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Returns the libpq endpoint, or an error if not available.
    ///
    /// This is a convenience method to avoid `unwrap()` calls when you need
    /// the endpoint and want proper error handling.
    ///
    /// # Errors
    ///
    /// Returns an error if this process was started in gRPC-only mode.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{HyperProcess, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let hyper = HyperProcess::new(None, None)?;
    ///     let endpoint = hyper.require_endpoint()?; // No unwrap() needed!
    ///     println!("Server running at: {}", endpoint);
    ///     Ok(())
    /// }
    /// ```
    pub fn require_endpoint(&self) -> crate::error::Result<&str> {
        self.endpoint().ok_or_else(|| {
            crate::error::Error::internal(
                "HyperProcess does not have a libpq endpoint (gRPC-only mode). \
                 Use grpc_endpoint() instead or start with LibPq or Both listen mode.",
            )
        })
    }

    /// Returns the gRPC endpoint for connecting to this instance.
    ///
    /// The endpoint is in the format "host:port" (e.g., "127.0.0.1:7484").
    ///
    /// Returns `None` if the process was started in libpq-only mode.
    #[must_use]
    pub fn grpc_endpoint(&self) -> Option<&str> {
        self.grpc_endpoint.as_deref()
    }

    /// Returns the gRPC endpoint, or an error if not available.
    ///
    /// This is a convenience method to avoid `unwrap()` calls when you need
    /// the gRPC endpoint and want proper error handling.
    ///
    /// # Errors
    ///
    /// Returns an error if this process was started in libpq-only mode.
    pub fn require_grpc_endpoint(&self) -> crate::error::Result<&str> {
        self.grpc_endpoint().ok_or_else(|| {
            crate::error::Error::internal(
                "HyperProcess does not have a gRPC endpoint (libpq-only mode). \
                 Use endpoint() instead or start with Grpc or Both listen mode.",
            )
        })
    }

    /// Returns the gRPC endpoint as a full URL suitable for gRPC clients.
    ///
    /// Returns the endpoint prefixed with "http://" (e.g., "<http://127.0.0.1:7484>").
    /// Returns `None` if the process was started in libpq-only mode.
    #[must_use]
    pub fn grpc_url(&self) -> Option<String> {
        self.grpc_endpoint.as_ref().map(|ep| format!("http://{ep}"))
    }

    /// Returns the gRPC URL, or an error if not available.
    ///
    /// This is a convenience method to avoid `unwrap()` calls when you need
    /// the gRPC URL and want proper error handling.
    ///
    /// # Errors
    ///
    /// Returns an error if this process was started in libpq-only mode.
    pub fn require_grpc_url(&self) -> crate::error::Result<String> {
        Ok(format!("http://{}", self.require_grpc_endpoint()?))
    }

    /// Returns the listen mode this process was started with.
    #[must_use]
    pub fn listen_mode(&self) -> ListenMode {
        self.listen_mode
    }

    /// Returns the transport mode this process was started with.
    #[must_use]
    pub fn transport_mode(&self) -> TransportMode {
        self.transport_mode
    }

    /// Returns the connection endpoint for this process.
    ///
    /// This returns a [`ConnectionEndpoint`] that can be used to connect
    /// to this Hyper instance via TCP, Unix Domain Socket, or Named Pipe.
    #[must_use]
    pub fn connection_endpoint(&self) -> Option<&ConnectionEndpoint> {
        self.connection_endpoint.as_ref()
    }

    /// Returns the log directory where hyperd writes its log files.
    ///
    /// The log file is typically `hyperd.log` within this directory.
    /// Returns `None` if the log directory could not be determined (e.g.,
    /// default parameters were disabled and no `log_dir` was specified).
    ///
    /// This is useful for setting up [`QueryStatsProvider`](crate::QueryStatsProvider)
    /// implementations that parse the Hyper log file.
    #[must_use]
    pub fn log_dir(&self) -> Option<&Path> {
        self.log_dir.as_deref()
    }

    /// Returns the socket directory used for UDS connections (Unix only).
    ///
    /// Returns `None` if the process is using TCP or if no socket directory was created.
    #[cfg(unix)]
    #[must_use]
    pub fn socket_directory(&self) -> Option<&Path> {
        self.socket_directory.as_deref()
    }

    /// Returns the pipe name used for Named Pipe connections (Windows only).
    ///
    /// Returns `None` if the process is using TCP.
    #[cfg(windows)]
    pub fn pipe_name(&self) -> Option<&str> {
        self.pipe_name.as_deref()
    }

    /// Returns the process ID of the Hyper server.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(std::process::Child::id)
    }

    /// Returns whether the Hyper server process is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        if let Some(ref child) = self.child {
            // Try to check if process is alive without waiting
            #[cfg(unix)]
            {
                match Command::new("kill")
                    .args(["-0", &child.id().to_string()])
                    .output()
                {
                    Ok(output) => output.status.success(),
                    Err(_) => false,
                }
            }
            #[cfg(not(unix))]
            {
                // On Windows, we can't easily check without waiting
                // Assume it's running if we have a handle
                let _ = child; // Silence unused variable warning
                true
            }
        } else {
            false
        }
    }

    /// Returns true if the hyperd child process has exited (or no child exists).
    ///
    /// Uses [`std::process::Child::try_wait`] under the hood, which is correct
    /// on both Unix and Windows. On Unix this also reaps any zombie state as a
    /// side effect — a hyperd that has been SIGKILLed but not yet `wait()`ed
    /// on by the parent will be observed as exited and cleaned up here.
    ///
    /// Prefer this over [`Self::is_running`] when the caller owns the
    /// `HyperProcess` mutably and needs an authoritative liveness signal.
    /// `is_running` uses `kill -0` on Unix (which incorrectly reports zombies
    /// as alive) and is a no-op on Windows.
    pub fn has_exited(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_status)) => true,
                Ok(None) => false,
                Err(_) => true,
            },
            None => true,
        }
    }

    /// Shuts down the Hyper server gracefully with a timeout.
    ///
    /// This closes the callback connection, which signals Hyper to shut down gracefully.
    /// If Hyper doesn't exit within the timeout, it will be forcefully terminated.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum time to wait for graceful shutdown before force-killing.
    ///
    /// # Errors
    ///
    /// Returns an error if the shutdown fails.
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Result<()> {
        self.shutdown_initiated.store(true, Ordering::SeqCst);
        self.do_shutdown(Some(timeout))
    }

    /// Shuts down the Hyper server gracefully, waiting indefinitely.
    ///
    /// This closes the callback connection and waits for Hyper to exit.
    /// Use [`shutdown_timeout`](Self::shutdown_timeout) if you need a timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if the shutdown fails.
    pub fn shutdown_graceful(mut self) -> Result<()> {
        self.shutdown_initiated.store(true, Ordering::SeqCst);
        self.do_shutdown(None)
    }

    /// Closes the callback connection to signal Hyper to shut down.
    ///
    /// This is the graceful shutdown mechanism - Hyper monitors the callback connection
    /// and will initiate shutdown when it's closed.
    fn close_callback_connection(&mut self) {
        if let Some(conn) = self.callback_connection.take() {
            // Gracefully shutdown both directions
            let _ = conn.shutdown(Shutdown::Both);
            // Connection is dropped here, closing the socket
        }
    }

    /// Internal shutdown implementation.
    fn do_shutdown(&mut self, timeout: Option<Duration>) -> Result<()> {
        info!(target: "hyperdb_api", "hyperd-shutdown");

        // Step 1: Close the callback connection to signal graceful shutdown
        // Hyper will detect this and begin shutting down
        self.close_callback_connection();

        if let Some(mut child) = self.child.take() {
            // Step 2: Wait for the process to exit
            let wait_result = if let Some(timeout) = timeout {
                // Wait with timeout
                let start = std::time::Instant::now();
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Ok(None) => {
                            if start.elapsed() > timeout {
                                // Step 3: Force kill ONLY after timeout
                                // This should rarely happen if Hyper is healthy
                                #[cfg(unix)]
                                {
                                    // Try SIGTERM first
                                    let _ = Command::new("kill")
                                        .args(["-TERM", &child.id().to_string()])
                                        .output();
                                    thread::sleep(Duration::from_millis(100));
                                }
                                // Then force kill
                                let _ = child.kill();
                                break child.wait().map_err(|e| {
                                    Error::internal(format!("Failed to wait for hyperd: {e}"))
                                });
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                        Err(e) => {
                            break Err(Error::internal(format!("Failed to wait for hyperd: {e}")))
                        }
                    }
                }
            } else {
                // Wait indefinitely
                child
                    .wait()
                    .map_err(|e| Error::internal(format!("Failed to wait for hyperd: {e}")))
            };

            wait_result?;
        }

        Ok(())
    }
}

impl Drop for HyperProcess {
    fn drop(&mut self) {
        if !self.shutdown_initiated.load(Ordering::SeqCst) {
            // Try to gracefully shutdown with a short timeout
            let _ = self.do_shutdown(Some(Duration::from_secs(5)));
        }

        // Clean up socket directory if we created one
        #[cfg(unix)]
        if let Some(ref dir) = self.socket_directory {
            // Only clean up if it's a temp directory we created (contains our PID)
            let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if dir_name.starts_with("hyper-") {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

// SAFETY: `HyperProcess` owns its `std::process::Child` handle and (optionally)
// a TCP connection. Both are themselves `Send`, and no field holds thread-local
// state or a non-`Send` raw pointer. Ownership of a `HyperProcess` therefore
// transfers cleanly across thread boundaries.
unsafe impl Send for HyperProcess {}

/// Special parameter key that disables default instance parameters when present.
///
/// By default, [`HyperProcess`] starts hyperd with a set of sensible default parameters
/// (matching the C++ `HyperProcess` behavior). If you need full control over all parameters,
/// include this key in your [`Parameters`] to disable all defaults.
pub(crate) const NO_DEFAULT_PARAMETERS: &str = "no_default_parameters";

/// Default log configuration for hyperd: file-based JSON logging.
const DEFAULT_LOG_CONFIG: &str = "file,json,all,hyperd,0";

/// Parameters for configuring the Hyper server.
///
/// When starting a [`HyperProcess`], a set of default parameters are automatically applied
/// (matching the C++ `HyperProcess` behavior):
///
/// | Parameter | Default Value | Description |
/// |-----------|---------------|-------------|
/// | `init_user` | `tableau_internal_user` | Initial user for the Hyper instance |
/// | `language` | `en_US` | Default language setting |
/// | `log_config` | `file,json,all,hyperd,0` | Log configuration |
/// | `date_style` | `MDY` | Date format (Month-Day-Year) |
/// | `date_style_lenient` | `false` | Strict date parsing |
/// | `log_dir` | Current directory | Log file directory |
/// | `no_password` | (flag) | Disable password requirement |
/// | `skip_license` | (flag) | Skip license check |
/// | `default_database_version` | `3` | File format version for newly created `.hyper` databases (v3 adds 128-bit NUMERIC support, required for DECIMAL128 parquet columns) |
///
/// To disable these defaults, add the `no_default_parameters` key (for example
/// `params.set("no_default_parameters", "")` via [`Parameters::set`].
///
/// # Listen Modes
///
/// Use [`set_listen_mode`](Parameters::set_listen_mode) to configure which protocols Hyper listens on:
///
/// ```
/// use hyperdb_api::{ListenMode, Parameters};
///
/// // gRPC only (for Arrow-based queries)
/// let mut params = Parameters::new();
/// params.set_listen_mode(ListenMode::Grpc { port: 0 });
///
/// // Both libpq and gRPC
/// let mut params = Parameters::new();
/// params.set_listen_mode(ListenMode::Both { grpc_port: 7484 });
/// ```
///
/// # Example
///
/// ```
/// use hyperdb_api::Parameters;
///
/// let mut params = Parameters::new();
/// params.set("log_file_size_limit", "100k");
/// params.set("log_file_max_count", "7");
/// ```
///
/// # Transport Modes
///
/// Use [`set_transport_mode`](Parameters::set_transport_mode) to control whether Hyper uses
/// TCP or IPC (Unix Domain Sockets):
///
/// ```
/// use hyperdb_api::{Parameters, TransportMode};
///
/// let mut params = Parameters::new();
/// params.set_transport_mode(TransportMode::Tcp); // Force TCP instead of IPC
/// ```
///
/// Transport mode for `HyperProcess` connections.
///
/// Controls whether the server uses TCP or Unix Domain Sockets (IPC) for connections.
/// On Unix systems, IPC is the default for better local performance.
/// On Windows, TCP is always used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportMode {
    /// Use IPC (Unix Domain Sockets on Unix, Named Pipes on Windows).
    /// This is the default mode and provides better performance for local connections.
    #[default]
    Ipc,

    /// Use TCP/IP connections.
    /// Required when connecting from remote clients or when IPC is not available.
    Tcp,
}

/// Parameters for configuring the Hyper server.
///
/// When starting a [`HyperProcess`], a set of default parameters are automatically applied
/// (matching the C++ `HyperProcess` behavior). You can override these defaults or disable
/// them entirely by adding the `no_default_parameters` key (for example
/// `params.set("no_default_parameters", "")` via [`Parameters::set`].
///
/// # Transport Modes
///
/// Use [`set_transport_mode`](Self::set_transport_mode) to control whether Hyper uses
/// TCP or IPC (Unix Domain Sockets on Unix systems).
///
/// # Example
///
/// ```
/// use hyperdb_api::{Parameters, TransportMode};
///
/// let mut params = Parameters::new();
/// params.set("log_file_size_limit", "100k");
/// params.set_transport_mode(TransportMode::Tcp); // Force TCP instead of IPC
/// ```
#[derive(Debug, Clone, Default)]
pub struct Parameters {
    values: Vec<(String, String)>,
    /// The listen mode for the Hyper server.
    pub(crate) listen_mode: Option<ListenMode>,
    /// The transport mode (TCP or IPC/UDS).
    pub(crate) transport_mode: Option<TransportMode>,
    /// Custom domain socket directory (Unix only).
    #[cfg(unix)]
    pub(crate) domain_socket_directory: Option<PathBuf>,
}

impl Parameters {
    /// Creates a new empty Parameters instance.
    #[must_use]
    pub fn new() -> Self {
        Parameters {
            values: Vec::new(),
            listen_mode: None,
            transport_mode: None,
            #[cfg(unix)]
            domain_socket_directory: None,
        }
    }

    /// Sets the transport mode (TCP or IPC/UDS).
    ///
    /// By default, `HyperProcess` uses IPC (Unix Domain Sockets on Unix) for better
    /// performance. Use `TransportMode::Tcp` if you need TCP connections.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::{Parameters, TransportMode};
    ///
    /// let mut params = Parameters::new();
    /// params.set_transport_mode(TransportMode::Tcp); // Use TCP instead of IPC
    /// ```
    pub fn set_transport_mode(&mut self, mode: TransportMode) -> &mut Self {
        self.transport_mode = Some(mode);
        self
    }

    /// Returns the configured transport mode.
    #[must_use]
    pub fn transport_mode(&self) -> Option<TransportMode> {
        self.transport_mode
    }

    /// Sets a custom domain socket directory (Unix only).
    ///
    /// By default, `HyperProcess` creates sockets in a temporary directory.
    /// Use this to specify a custom location.
    #[cfg(unix)]
    pub fn set_domain_socket_directory(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.domain_socket_directory = Some(dir.into());
        self
    }

    /// Returns the configured domain socket directory (Unix only).
    #[cfg(unix)]
    #[must_use]
    pub fn domain_socket_directory(&self) -> Option<&Path> {
        self.domain_socket_directory.as_deref()
    }

    /// Sets a parameter value.
    ///
    /// # Arguments
    ///
    /// * `key` - The parameter name.
    /// * `value` - The parameter value (empty string for flags).
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        let key = key.into();
        let value = value.into();

        // Update existing or add new
        if let Some(entry) = self.values.iter_mut().find(|(k, _)| k == &key) {
            entry.1 = value;
        } else {
            self.values.push((key, value));
        }

        self
    }

    /// Sets the listen mode for the Hyper server.
    ///
    /// This controls which protocols the server listens on:
    /// - [`ListenMode::LibPq`]: `PostgreSQL` wire protocol only (default)
    /// - [`ListenMode::Grpc`]: gRPC protocol only (query-only, Arrow results)
    /// - [`ListenMode::Both`]: Both protocols enabled
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::{ListenMode, Parameters};
    ///
    /// let mut params = Parameters::new();
    /// params.set_listen_mode(ListenMode::Grpc { port: 0 }); // Auto-assign port
    /// ```
    pub fn set_listen_mode(&mut self, mode: ListenMode) -> &mut Self {
        self.listen_mode = Some(mode);
        self
    }

    /// Returns the configured listen mode, if any.
    #[must_use]
    pub fn listen_mode(&self) -> Option<ListenMode> {
        self.listen_mode
    }

    /// Gets a parameter value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Returns whether the parameters contain the given key.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.values.iter().any(|(k, _)| k == key)
    }

    /// Returns an iterator over the parameters.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Returns whether the parameters are empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns the number of parameters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parameters() {
        let mut params = Parameters::new();
        params.set("key1", "value1");
        params.set("key2", "value2");

        assert_eq!(params.get("key1"), Some("value1"));
        assert_eq!(params.get("key2"), Some("value2"));
        assert_eq!(params.get("key3"), None);
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn test_parameters_update() {
        let mut params = Parameters::new();
        params.set("key", "value1");
        params.set("key", "value2");

        assert_eq!(params.get("key"), Some("value2"));
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn test_parse_connection_descriptor() {
        assert_eq!(
            HyperProcess::parse_connection_descriptor("tab.tcp://localhost:12345").unwrap(),
            "localhost:12345"
        );
        assert_eq!(
            HyperProcess::parse_connection_descriptor("tab.tcp://127.0.0.1:7483").unwrap(),
            "127.0.0.1:7483"
        );
        assert_eq!(
            HyperProcess::parse_connection_descriptor("tcp://localhost:8080").unwrap(),
            "localhost:8080"
        );
        // Already in host:port format
        assert_eq!(
            HyperProcess::parse_connection_descriptor("localhost:9999").unwrap(),
            "localhost:9999"
        );
    }

    #[test]
    fn test_parse_connection_descriptor_named_pipe() {
        assert_eq!(
            HyperProcess::parse_connection_descriptor("tab.pipe://./pipe/hyper-12345").unwrap(),
            r"\\.\pipe\hyper-12345"
        );
        assert_eq!(
            HyperProcess::parse_connection_descriptor("tab.pipe://server1/pipe/mydb").unwrap(),
            r"\\server1\pipe\mydb"
        );
    }

    #[test]
    fn test_parse_connection_descriptor_invalid() {
        assert!(HyperProcess::parse_connection_descriptor("").is_err());
        assert!(HyperProcess::parse_connection_descriptor("invalid").is_err());
    }

    #[test]
    fn test_parameters_contains_key() {
        let mut params = Parameters::new();
        params.set("key1", "value1");

        assert!(params.contains_key("key1"));
        assert!(!params.contains_key("key2"));
    }

    #[test]
    fn test_no_default_parameters_constant() {
        // Verify the constant matches what C++ uses
        assert_eq!(NO_DEFAULT_PARAMETERS, "no_default_parameters");
    }

    #[test]
    fn test_parameters_with_no_defaults() {
        let mut params = Parameters::new();
        params.set(NO_DEFAULT_PARAMETERS, "");
        params.set("init_user", "custom_user");

        assert!(params.contains_key(NO_DEFAULT_PARAMETERS));
        assert_eq!(params.get("init_user"), Some("custom_user"));
    }
}
