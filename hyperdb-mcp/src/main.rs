// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Binary entry point for the `hyperdb-mcp` MCP server.
//!
//! Starts an MCP server on stdio with a local database and optional persistent
//! attachment.
//! Can also run in daemon mode to manage a shared `hyperd` process.
//!
//! # Logging
//!
//! Runtime events go to two places:
//!
//! 1. **stderr** — controlled by `RUST_LOG` (filters like `info` or `debug`).
//!    MCP clients typically capture stderr and surface it as plugin output.
//!    Never pollutes stdout, which carries the JSON-RPC protocol.
//! 2. **`<log_dir>/hyperdb-mcp.log`** — append-only file, same log filter.
//!    The path is reported in the `status` tool's `logs.client_log` field.
//!
//! Both `hyperd` and the client write to the same `log_dir` (see
//! [`hyperdb_mcp::engine::resolve_log_dir`]). Check the `status` tool for
//! the exact paths.

use clap::{Parser, Subcommand};
use hyperdb_mcp::daemon;
use hyperdb_mcp::daemon::discovery;
use hyperdb_mcp::daemon::health;
use hyperdb_mcp::daemon::run::DaemonConfig;
use hyperdb_mcp::diagnostics::{self, DoctorOptions};
use hyperdb_mcp::engine::{resolve_log_dir, CLIENT_LOG_FILE_NAME};
use hyperdb_mcp::paths;
use hyperdb_mcp::server::HyperMcpServer;
use rmcp::ServiceExt;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

// Both MCP_VERSION and HYPERDB_GIT_HASH are env! string literals, so this
// concat! resolves at compile time into a single &'static str — exactly
// what clap wants for the `version = ...` attribute.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), ".r", env!("HYPERDB_GIT_HASH"));

#[derive(Parser)]
#[command(
    name = "hyperdb-mcp",
    version = VERSION,
    about = "MCP server for Hyper database analytics",
    long_about = "MCP server for Hyper database analytics. HYPERD_PATH accepts either the hyperd executable or its containing directory. When HYPERD_PATH is absent or non-UTF-8, runtime resolution searches upward through current-directory ancestors for .hyperd/current/hyperd; no general PATH lookup is performed."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to the persistent `.hyper` file. Defaults to the platform
    /// data dir (e.g. `~/Library/Application Support/hyperdb/workspace.hyper`
    /// on macOS) or the `HYPERDB_PERSISTENT_DB` env var if set.
    #[arg(long, global = true)]
    persistent_db: Option<String>,

    /// DEPRECATED alias for `--persistent-db`. Will be removed in a
    /// future release.
    #[arg(long, global = true, hide = true)]
    workspace: Option<String>,

    /// Skip opening any persistent database. The session has only the
    /// local database plus any user-attached databases. Disables
    /// `save_query` persistence (queries fall back to session storage).
    #[arg(long, global = true)]
    ephemeral_only: bool,

    /// Run in read-only mode. Guards `execute`, `load_data`, `load_file`,
    /// `load_files`, `load_iceberg`, `watch_directory`, `save_query`,
    /// `delete_query`, `set_table_metadata`, `copy_query`, `kv_set`,
    /// `kv_set_many`, `kv_delete`, `kv_pop`, `kv_clear`, and writable/create
    /// `attach_database`. Read-only `attach_database` remains available;
    /// `unwatch_directory` and `export` (including Hyper format) stay available.
    #[arg(long, global = true)]
    read_only: bool,

    /// Disable the shared daemon and spawn a private `hyperd` (legacy behavior)
    #[arg(long, global = true)]
    no_daemon: bool,
}

impl Cli {
    fn validate_persistent_options(&self) -> Result<(), &'static str> {
        if self.ephemeral_only && (self.persistent_db.is_some() || self.workspace.is_some()) {
            return Err("--ephemeral-only is incompatible with --persistent-db / --workspace");
        }
        if self.persistent_db.is_some() && self.workspace.is_some() {
            return Err("Both --persistent-db and --workspace were supplied. \
                 --workspace is a deprecated alias; pass only --persistent-db.");
        }
        Ok(())
    }

    /// Translate the deprecated `--workspace` flag to `--persistent-db`,
    /// emitting a one-time deprecation warning, and resolve the final
    /// persistent path according to the precedence rules in
    /// [`paths::resolve_persistent_db_path`]. Returns `None` only when
    /// `--ephemeral-only` is set.
    ///
    /// Errors out if both `--persistent-db` and `--workspace` are
    /// supplied — there's no sensible "winner", so be loud about it.
    fn resolve_persistent_path(&self) -> Result<Option<std::path::PathBuf>, &'static str> {
        self.validate_persistent_options()?;
        if self.workspace.is_some() {
            eprintln!(
                "warning: --workspace is deprecated; use --persistent-db instead. \
                 The old flag will be removed in a future release."
            );
        }
        Ok(paths::resolve_persistent_db_path_with_source(
            self.persistent_db.as_deref(),
            self.workspace.as_deref(),
            self.ephemeral_only,
        )
        .observed_path)
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Side-effect-free installation/configuration/identity diagnostics; starts no Hyper or database
    Doctor {
        /// Emit the typed report as JSON
        #[arg(long)]
        json: bool,
    },

    /// Run a foreground daemon managing shared hyperd. Auto-spawn scans before
    /// launching; foreground startup binds its configured/base port exactly.
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,

        /// Exact TCP health/lock port for foreground startup. Without `--port`,
        /// the foreground daemon binds the configured/base port exactly
        /// (`HYPERDB_DAEMON_PORT` when valid, otherwise 7485) and does not scan.
        /// Auto-spawn performs bounded discovery from its configured base before
        /// launching. For stop/status, omitting the port uses discovery plus
        /// scanning.
        #[arg(long, global = true)]
        port: Option<u16>,

        /// Idle timeout in seconds before the daemon shuts down
        #[arg(long)]
        idle_timeout: Option<u64>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Stop a running daemon
    Stop,
    /// Show status of the running daemon
    Status,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cli = Cli::parse();
    let command = cli.command.take();

    match command {
        Some(Commands::Doctor { json }) => run_doctor_mode(&cli, json),
        Some(Commands::Daemon {
            action: Some(DaemonAction::Stop),
            port,
            ..
        }) => {
            daemon_stop(port);
            Ok(())
        }
        Some(Commands::Daemon {
            action: Some(DaemonAction::Status),
            port,
            ..
        }) => {
            daemon_status(port);
            Ok(())
        }
        Some(Commands::Daemon {
            action: None,
            port,
            idle_timeout,
        }) => {
            // Resolve the effective port for daemon startup
            let effective_port = port.unwrap_or_else(|| discovery::resolve_port_scan().base);
            run_daemon_mode(effective_port, idle_timeout).await
        }
        None => run_mcp_mode(cli).await,
    }
}

fn run_doctor_mode(cli: &Cli, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    if let Err(message) = cli.validate_persistent_options() {
        eprintln!("error: {message}");
        std::process::exit(2);
    }
    let report = diagnostics::collect_doctor_report(DoctorOptions {
        persistent_db: cli.persistent_db.as_deref(),
        deprecated_workspace: cli.workspace.as_deref(),
        ephemeral_only: cli.ephemeral_only,
        read_only: cli.read_only,
        no_daemon: cli.no_daemon,
    })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", diagnostics::render_doctor_human(&report));
    }
    Ok(())
}

async fn run_daemon_mode(
    port: u16,
    idle_timeout: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Daemon logs go to ~/.hyperdb/logs/
    let log_dir = discovery::state_dir()?.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::never(&log_dir, "hyperdb-daemon.log");
    let (file_writer, _file_guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyperdb_mcp=debug"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false))
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .init();

    let config = DaemonConfig::from_args(port, idle_timeout);
    daemon::run::run_daemon(config).await
}

async fn run_mcp_mode(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let persistent_path = match cli.resolve_persistent_path() {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(2);
        }
    };
    // Pass the resolved path to log-dir resolution: ephemeral-only
    // sessions land in the per-pid temp dir.
    let persistent_str = persistent_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let log_dir = resolve_log_dir(persistent_str.as_deref());
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "warning: failed to create log directory {}: {e} — client logs will go to stderr only",
            log_dir.display()
        );
    }

    let file_appender = tracing_appender::rolling::never(&log_dir, CLIENT_LOG_FILE_NAME);
    let (file_writer, _file_guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyperdb_mcp=debug"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false))
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .init();

    tracing::info!(
        log_dir = %log_dir.display(),
        persistent_db = persistent_str.as_deref().unwrap_or("<ephemeral-only>"),
        read_only = cli.read_only,
        ephemeral_only = cli.ephemeral_only,
        no_daemon = cli.no_daemon,
        "hyperdb-mcp starting"
    );

    let server = HyperMcpServer::with_no_daemon(persistent_str, cli.read_only, cli.no_daemon);
    // Eagerly initialize the engine before accepting tool calls so observer
    // tools like `status` report full stats on the first call (issue #138).
    // Errors are logged and swallowed inside `warm_up_engine` — startup
    // proceeds even if hyperd is momentarily unreachable. Run on a blocking
    // thread: warm-up does synchronous I/O (and may spawn the daemon) and
    // would otherwise stall a runtime worker. Nothing else runs on the
    // runtime yet (serve() is below), so this only delays startup.
    let server = tokio::task::spawn_blocking(move || {
        server.warm_up_engine();
        server
    })
    .await?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;

    Ok(())
}

fn daemon_stop(port: Option<u16>) {
    let target_port = match port {
        Some(p) => p,
        None => {
            // No explicit port — discover the running daemon
            if let Some(info) = discovery::find_running_daemon() {
                info.health_port
            } else {
                eprintln!("No daemon is currently running.");
                std::process::exit(1);
            }
        }
    };

    match health::send_command(target_port, "STOP") {
        Ok(response) => {
            println!("Daemon responded: {}", response.trim());
        }
        Err(e) => {
            eprintln!("No daemon running on port {target_port} (or cannot connect): {e}");
            std::process::exit(1);
        }
    }
}

fn daemon_status(port: Option<u16>) {
    let info = if let Some(port) = port {
        match health::send_command(port, "STATUS") {
            Ok(response) => match serde_json::from_str::<discovery::DaemonInfo>(response.trim()) {
                Ok(info) => info,
                Err(e) => {
                    eprintln!("Daemon on port {port} returned invalid status: {e}");
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("No daemon running on port {port} (or cannot connect): {e}");
                std::process::exit(1);
            }
        }
    } else if let Some(info) = discovery::find_running_daemon() {
        info
    } else {
        eprintln!("No daemon is currently running.");
        std::process::exit(1);
    };

    println!("Daemon is running:");
    println!("  PID:            {}", info.pid);
    println!("  Hyperd endpoint: {}", info.hyperd_endpoint);
    println!("  Health port:    {}", info.health_port);
    println!("  Started:        {}", info.started_at);
    println!("  Version:        {}", info.version);
}
