// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Binary entry point for the `hyperdb-mcp` MCP server.
//!
//! Starts an MCP server on stdio, optionally backed by a persistent workspace.
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
    about = "MCP server for Hyper database analytics"
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
    /// ephemeral primary plus any user-attached databases. Disables
    /// `save_query` persistence (queries fall back to session storage).
    #[arg(long, global = true)]
    ephemeral_only: bool,

    /// Run in read-only mode: disables execute, `load_data`, `load_file`, and export to hyper format
    #[arg(long, global = true)]
    read_only: bool,

    /// Disable the shared daemon and spawn a private `hyperd` (legacy behavior)
    #[arg(long, global = true)]
    no_daemon: bool,
}

impl Cli {
    /// Translate the deprecated `--workspace` flag to `--persistent-db`,
    /// emitting a one-time deprecation warning, and resolve the final
    /// persistent path according to the precedence rules in
    /// [`paths::resolve_persistent_db_path`]. Returns `None` only when
    /// `--ephemeral-only` is set.
    ///
    /// Errors out if both `--persistent-db` and `--workspace` are
    /// supplied — there's no sensible "winner", so be loud about it.
    fn resolve_persistent_path(&self) -> Result<Option<std::path::PathBuf>, &'static str> {
        if self.ephemeral_only {
            if self.persistent_db.is_some() || self.workspace.is_some() {
                return Err("--ephemeral-only is incompatible with --persistent-db / --workspace");
            }
            return Ok(None);
        }
        if self.persistent_db.is_some() && self.workspace.is_some() {
            return Err("Both --persistent-db and --workspace were supplied. \
                 --workspace is a deprecated alias; pass only --persistent-db.");
        }
        if self.workspace.is_some() {
            eprintln!(
                "warning: --workspace is deprecated; use --persistent-db instead. \
                 The old flag will be removed in a future release."
            );
        }
        let cli_value = self.persistent_db.as_deref().or(self.workspace.as_deref());
        Ok(paths::resolve_persistent_db_path(cli_value))
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Run as a background daemon managing a shared hyperd process
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,

        /// TCP port for health listener and single-instance lock. When omitted,
        /// the daemon scans from the base port to find a free port. For stop/status
        /// commands, omitting the port uses discovery + scanning to find the running daemon.
        #[arg(long)]
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
    let cli = Cli::parse();

    match cli.command {
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
            ..
        }) => {
            daemon_status();
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

fn daemon_status() {
    if let Some(info) = discovery::find_running_daemon() {
        println!("Daemon is running:");
        println!("  PID:            {}", info.pid);
        println!("  Hyperd endpoint: {}", info.hyperd_endpoint);
        println!("  Health port:    {}", info.health_port);
        println!("  Started:        {}", info.started_at);
        println!("  Version:        {}", info.version);
    } else {
        eprintln!("No daemon is currently running.");
        std::process::exit(1);
    }
}
