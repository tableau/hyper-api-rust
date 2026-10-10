// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

#![allow(
    missing_docs,
    reason = "Primarily an MCP server binary; the library target exists to support it and is not a documented API surface. Tool-level docs are surfaced via the MCP protocol, not rustdoc."
)]

//! MCP (Model Context Protocol) server that exposes the Hyper columnar database
//! as an instant SQL analytics engine for LLM workflows.
//!
//! # This library target is not a public API
//!
//! `hyperdb-mcp` ships as an MCP server binary. The library target exists to
//! support that binary, its integration tests, and its examples — it is **not**
//! a supported Rust API surface, and no item in it is covered by the crate's
//! semantic-versioning promise. Every module below is therefore
//! `#[doc(hidden)]`.
//!
//! What *is* stable and semver-governed is the **MCP tool surface**: tool
//! names, their parameters, and their behavior as reached over the MCP
//! protocol. That is the contract described in the crate
//! [README](https://github.com/tableau/hyper-api-rust/blob/main/hyperdb-mcp/README.md),
//! and the `get_readme` tool serves the LLM-facing version of it at runtime.
//!
//! The modules stay `pub` rather than `pub(crate)` because Cargo compiles the
//! `hyperdb-mcp` binary, every file under `tests/`, and every example as
//! separate crates, which can only reach library items through the external
//! `hyperdb_mcp::` path. All 21 modules are used by at least one of those, so
//! `pub` is load-bearing for compilation, not an API commitment.
//!
//! # Architecture
//!
//! The crate is layered bottom-up:
//!
//! - `daemon` — Single-instance daemon that shares one `hyperd` across MCP clients (discovery, spawn, health listener).
//! - `paths` — Platform-standard default path for the persistent database.
//! - `error` — Structured error codes with recovery suggestions for LLM self-correction.
//! - `attach` — Registry of additional `.hyper` databases attached to the primary
//!   workspace for cross-database JOINs and `copy_query`. Replays attachments
//!   after a `ConnectionLost` reconnect.
//! - `stats` — Performance telemetry (throughput, timing) attached to every response.
//! - `schema` — Three-tier schema inference: exact (Arrow/Parquet), structural (JSON),
//!   heuristic (CSV). Also handles user-provided schema overrides.
//! - `engine` — Manages the `HyperProcess` lifecycle, connection, table CRUD, and
//!   query execution across the local database and optional persistent database.
//! - `ingest` — Loads inline JSON (row-by-row INSERT) and CSV (`COPY FROM`) into Hyper.
//! - `ingest_arrow` — Loads Parquet and Arrow IPC files via the Arrow crate.
//! - `lakehouse` — Loads Apache Iceberg tables via hyperd's native `external(..., format => 'iceberg')` scan.
//! - `inspect` — Dry-run file inspection powering the `inspect_file` MCP tool.
//! - `export` — Writes query results to CSV, Parquet, Arrow IPC, Iceberg, or `.hyper` files.
//! - `chart` — Renders SQL query results as PNG/SVG charts via the `plotters` crate.
//! - `saved_queries` — Named read-only SQL queries exposed via tools and `hyper://queries/...` resources.
//! - `subscriptions` — Per-URI registry of MCP clients that asked for resource-update notifications.
//! - `table_catalog` — User-visible catalog of data tables (`_table_catalog`) tracking
//!   source, purpose, and load history so workspaces are self-documenting.
//! - `version` — Compile-time-captured version strings for the MCP crate and the underlying `hyperdb-api`, with a git-hash suffix.
//! - `readme` — Static LLM-facing README returned by the `get_readme` tool.
//! - `watcher` — Monitors directories for incremental ingest via a `.ready` sentinel protocol.
//! - `server` — MCP tool definitions and the `rmcp` server handler that ties everything together.
//! - `diagnostics` — Installation and launcher identity, and the `doctor` report.

// Every module below is `#[doc(hidden)]`: reachable so the binary, the
// integration tests, and the examples compile, but excluded from the published
// rustdoc because the Rust surface is not the crate's supported API. See the
// crate-level docs above.
#[doc(hidden)]
pub mod attach;
#[doc(hidden)]
pub mod chart;
#[doc(hidden)]
pub mod daemon;
#[doc(hidden)]
pub mod diagnostics;
#[doc(hidden)]
pub mod engine;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod export;
mod file_identity;
#[doc(hidden)]
pub mod ingest;
#[doc(hidden)]
pub mod ingest_arrow;
#[doc(hidden)]
pub mod inspect;
#[doc(hidden)]
pub mod lakehouse;
#[doc(hidden)]
pub mod paths;
#[doc(hidden)]
pub mod readme;
#[doc(hidden)]
pub mod saved_queries;
#[doc(hidden)]
pub mod schema;
#[doc(hidden)]
pub mod server;
mod sql_classify;
#[doc(hidden)]
pub mod stats;
#[doc(hidden)]
pub mod subscriptions;
#[doc(hidden)]
pub mod table_catalog;
#[doc(hidden)]
pub mod version;
#[doc(hidden)]
pub mod watcher;
