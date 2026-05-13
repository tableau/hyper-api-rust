// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Binary entry point for the `hyperdb-mcp` MCP server.
//!
//! Starts an MCP server on stdio, optionally backed by a persistent workspace.
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

use clap::Parser;
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
    /// Path to the `.hyper` workspace file for persistent mode (omit for ephemeral mode)
    #[arg(long)]
    workspace: Option<String>,

    /// Run in read-only mode: disables execute, `load_data`, `load_file`, and export to hyper format
    #[arg(long)]
    read_only: bool,

    /// Bare mode: disable MCP-managed auxiliary tables. Skips creating
    /// `_table_catalog` and forces saved queries into in-memory
    /// (non-persistent) storage, even with --workspace.
    #[arg(long)]
    bare: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse CLI first so the log directory matches whatever workspace the
    // user requested. This has to happen before tracing is initialized so
    // the file layer points at the right place.
    let cli = Cli::parse();

    // Compute and create the log dir (same logic Engine will use later).
    let log_dir = resolve_log_dir(cli.workspace.as_deref());
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "warning: failed to create log directory {}: {e} — client logs will go to stderr only",
            log_dir.display()
        );
    }

    // tracing_appender writes via a background thread; we keep the guard
    // alive for the duration of `main` so buffered logs get flushed cleanly.
    let file_appender = tracing_appender::rolling::never(&log_dir, CLIENT_LOG_FILE_NAME);
    let (file_writer, _file_guard) = tracing_appender::non_blocking(file_appender);

    // Default to `info` when RUST_LOG is unset so the log files are actually
    // populated. Users can still override via RUST_LOG=debug,hyperdb_api=trace etc.
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
        "hyperdb-mcp starting"
    );

    let server = HyperMcpServer::new(cli.workspace, cli.read_only, cli.bare);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;

    Ok(())
}
