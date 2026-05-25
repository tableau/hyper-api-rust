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

    /// Path to the `.hyper` workspace file for persistent mode (omit for ephemeral mode)
    #[arg(long, global = true)]
    workspace: Option<String>,

    /// Run in read-only mode: disables execute, `load_data`, `load_file`, and export to hyper format
    #[arg(long, global = true)]
    read_only: bool,

    /// Bare mode: disable MCP-managed auxiliary tables. Skips creating
    /// `_table_catalog` and forces saved queries into in-memory
    /// (non-persistent) storage, even with --workspace.
    #[arg(long, global = true)]
    bare: bool,

    /// Disable the shared daemon and spawn a private `hyperd` (legacy behavior)
    #[arg(long, global = true)]
    no_daemon: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as a background daemon managing a shared hyperd process
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,

        /// TCP port for health listener and single-instance lock
        #[arg(long, default_value_t = daemon::DEFAULT_DAEMON_PORT)]
        port: u16,

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
        }) => run_daemon_mode(port, idle_timeout).await,
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
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .init();

    let config = DaemonConfig::from_args(port, idle_timeout);
    daemon::run::run_daemon(config).await
}

async fn run_mcp_mode(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let log_dir = resolve_log_dir(cli.workspace.as_deref());
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
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .init();

    tracing::info!(
        log_dir = %log_dir.display(),
        workspace = cli.workspace.as_deref().unwrap_or("<ephemeral>"),
        read_only = cli.read_only,
        bare = cli.bare,
        no_daemon = cli.no_daemon,
        "hyperdb-mcp starting"
    );

    let server =
        HyperMcpServer::with_no_daemon(cli.workspace, cli.read_only, cli.bare, cli.no_daemon);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;

    Ok(())
}

fn daemon_stop(port: u16) {
    match health::send_command(port, "STOP") {
        Ok(response) => {
            println!("Daemon responded: {}", response.trim());
        }
        Err(e) => {
            eprintln!("No daemon running on port {port} (or cannot connect): {e}");
            std::process::exit(1);
        }
    }
}

fn daemon_status() {
    if let Some(info) = discovery::discover() {
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
