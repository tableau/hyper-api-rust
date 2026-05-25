// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

#![allow(
    missing_docs,
    reason = "MCP server binary crate; not published to crates.io. Tool-level docs are surfaced via the MCP protocol, not rustdoc."
)]

//! MCP (Model Context Protocol) server that exposes the Hyper columnar database
//! as an instant SQL analytics engine for LLM workflows.
//!
//! # Architecture
//!
//! The crate is layered bottom-up:
//!
//! - [`error`] — Structured error codes with recovery suggestions for LLM self-correction.
//! - [`attach`] — Registry of additional `.hyper` databases attached to the primary
//!   workspace for cross-database JOINs and `copy_query`. Replays attachments
//!   after a `ConnectionLost` reconnect.
//! - [`stats`] — Performance telemetry (throughput, timing) attached to every response.
//! - [`schema`] — Three-tier schema inference: exact (Arrow/Parquet), structural (JSON),
//!   heuristic (CSV). Also handles user-provided schema overrides.
//! - [`engine`] — Manages the `HyperProcess` lifecycle, connection, table CRUD, and
//!   query execution. Supports ephemeral and persistent workspace modes.
//! - [`ingest`] — Loads inline JSON (row-by-row INSERT) and CSV (`COPY FROM`) into Hyper.
//! - [`ingest_arrow`] — Loads Parquet and Arrow IPC files via the Arrow crate.
//! - [`inspect`] — Dry-run file inspection powering the `inspect_file` MCP tool.
//! - [`export`] — Writes query results to CSV, Parquet, Arrow IPC, or `.hyper` files.
//! - [`chart`] — Renders SQL query results as PNG/SVG charts via the `plotters` crate.
//! - [`saved_queries`] — Named read-only SQL queries exposed via tools and `hyper://queries/...` resources.
//! - [`subscriptions`] — Per-URI registry of MCP clients that asked for resource-update notifications.
//! - [`table_catalog`] — User-visible catalog of data tables (`_table_catalog`) tracking
//!   source, purpose, and load history so workspaces are self-documenting. Disabled by `--bare`.
//! - [`version`] — Compile-time-captured version strings for the MCP crate and the underlying `hyperdb-api`, with a git-hash suffix.
//! - [`readme`] — Static LLM-facing README returned by the `get_readme` tool.
//! - [`watcher`] — Monitors directories for incremental ingest via a `.ready` sentinel protocol.
//! - [`server`] — MCP tool definitions and the `rmcp` server handler that ties everything together.

pub mod attach;
pub mod chart;
pub mod daemon;
pub mod engine;
pub mod error;
pub mod export;
pub mod ingest;
pub mod ingest_arrow;
pub mod inspect;
pub mod lakehouse;
pub mod readme;
pub mod saved_queries;
pub mod schema;
pub mod server;
pub mod stats;
pub mod subscriptions;
pub mod table_catalog;
pub mod version;
pub mod watcher;
