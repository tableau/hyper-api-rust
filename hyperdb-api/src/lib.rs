// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Pure Rust API for Hyper database.
//!
//! This crate provides a safe, idiomatic Rust interface for working with
//! Hyper database files (.hyper). It is a pure-Rust implementation using
//! the `PostgreSQL` wire protocol with Hyper-specific extensions.
//!
//! # Architecture
//!
//! This crate is the user-facing layer over `hyperdb-api-core`, an internal
//! implementation crate (not a public API) with three modules: `types`
//! (`LittleEndian` encoding), `protocol` (wire protocol, `HyperBinary` COPY)
//! and `client` (sync/async TCP, UDS, named-pipe and gRPC clients).
//!
//! Optional companion crates:
//! - `sea-query-hyperdb` — `HyperDB` SQL dialect backend for `sea-query`
//! - `hyperdb-api-salesforce` — Salesforce Data Cloud OAuth authentication
//! - `hyperdb-api-derive` — `#[derive(FromRow)]`, `#[derive(Table)]`,
//!   `query_as!` and `query_scalar!`; add it as a direct dependency (it is not
//!   re-exported here)
//!
//! # Quick Start
//!
//! ```no_run
//! use hyperdb_api::{HyperProcess, Connection, CreateMode, Result};
//!
//! fn main() -> Result<()> {
//!     let hyper = HyperProcess::new(None, None)?;
//!     let conn = Connection::new(&hyper, "example.hyper", CreateMode::CreateIfNotExists)?;
//!
//!     conn.execute_command("CREATE TABLE test (id INT, name TEXT)")?;
//!     conn.execute_command("INSERT INTO test VALUES (1, 'Hello')")?;
//!
//!     let mut result = conn.execute_query("SELECT * FROM test")?;
//!     while let Some(chunk) = result.next_chunk()? {
//!         for row in &chunk {
//!             let id: Option<i32> = row.get(0);
//!             let name: Option<String> = row.get(1);
//!             println!("id: {id:?}, name: {name:?}");
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Lifetime Safety
//!
//! The API uses lifetime annotations to provide compile-time guarantees that
//! resources are used correctly. All dependent types ([`Inserter`],
//! [`Catalog`], [`Rowset`], [`Transaction`]) carry a `'conn` lifetime
//! parameter tying them to the [`Connection`] they borrow:
//!
//! ```text
//! Connection (owns underlying client)
//! ├── Inserter<'conn>
//! ├── Catalog<'conn>
//! ├── KvStore<'conn>
//! ├── Rowset<'conn>
//! └── Transaction<'conn>
//! ```
//!
//! This is a **simple hierarchical design**, not a complex lifetime web:
//! - **Single root owner**: `Connection` owns the underlying client
//! - **Simple borrows**: Dependent types borrow the `Connection`; a
//!   [`Transaction`] borrows it exclusively
//! - **No circular references**: `Inserter` doesn't reference `Catalog`, etc.
//! - **Single lifetime parameter**: Just one `'conn` — no multi-lifetime bounds
//!
//! The Rust borrow checker enforces that you cannot drop or move a `Connection`
//! while any dependent type holds a reference to it:
//!
//! ```compile_fail
//! # use hyperdb_api::{Connection, Inserter, CreateMode, SqlType, TableDefinition};
//! # fn example() -> hyperdb_api::Result<()> {
//! let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
//! let def = TableDefinition::new("t").add_required_column("a", SqlType::int());
//! let mut inserter = Inserter::new(&conn, &def)?;
//! drop(conn);  // ERROR: cannot move out of `conn` because it is borrowed
//! inserter.execute()?;
//! # Ok(())
//! # }
//! ```
//!
//! The same guarantee holds for a [`KvStore`], which borrows its `Connection`
//! for the handle's lifetime:
//!
//! ```compile_fail
//! # use hyperdb_api::{Connection, CreateMode};
//! # fn example() -> hyperdb_api::Result<()> {
//! let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
//! let kv = conn.kv_store("s")?;
//! drop(conn);  // ERROR: cannot move `conn` because it is borrowed by `kv`
//! let _ = kv.get("k")?;
//! # Ok(())
//! # }
//! ```
//!
//! [`Inserter::execute`] takes `&mut self`; the borrow of the [`Connection`]
//! ends when the [`Inserter`] is dropped.
//!
//! # Key Types
//!
//! - [`Connection`] / [`AsyncConnection`] — Sync and async database connections
//! - [`HyperProcess`] — Manage a local `hyperd` server process
//! - [`Inserter`] / [`MappedInserter`] / [`AsyncInserter`] — Bulk row insertion (`HyperBinary` COPY)
//! - [`ArrowInserter`] / [`AsyncArrowInserter`] — Arrow `RecordBatch` insertion
//! - [`Catalog`] — Schema/table introspection
//! - [`TableDefinition`] — Define table schemas
//! - [`Transaction`] / [`AsyncTransaction`] — RAII transaction guards
//! - [`KvStore`] / [`AsyncKvStore`] — String-native key-value store over a shared backing table
//!
//! # Public Modules
//!
//! - [`copy`] — CSV/text export and import via COPY protocol
//! - [`pool`] — Async connection pooling
//! - [`grpc`] — gRPC transport types for Arrow IPC queries
//!
//! # Bulk Data Loading
//!
//! Several inserter APIs are available depending on your data format and runtime model:
//! - [`Inserter`] / [`MappedInserter`] — Sync `HyperBinary` row-by-row
//! - [`AsyncInserter`] — Async `HyperBinary` row-by-row (mirrors [`Inserter`])
//! - [`ArrowInserter`] — Sync Arrow IPC (batch or streaming `RecordBatch`)
//! - [`AsyncArrowInserter`] — Async Arrow IPC
//! - [`copy`] module — CSV/TSV/delimited text import & export
//!
//! # Authentication
//!
//! The client supports multiple authentication methods (Trust, Cleartext, MD5, SCRAM-SHA-256):
//!
//! ```no_run
//! use hyperdb_api::{Connection, CreateMode, Result};
//!
//! fn main() -> Result<()> {
//!     let conn = Connection::connect_with_auth(
//!         "localhost:7483",
//!         "example.hyper",
//!         CreateMode::CreateIfNotExists,
//!         "myuser",
//!         "mypassword",
//!     )?;
//!     Ok(())
//! }
//! ```

#![warn(missing_docs, rust_2018_idioms, clippy::all)]
// `must_use_candidate` is `allow` workspace-wide because it measures
// *public API* ergonomics, and 140 of its 141 workspace-wide sites are in
// places where that does not apply: `hyperdb-api-core` (explicitly not a
// public API), the `hyperdb-mcp` binary's internal daemon helpers, and
// prost-generated protobuf code. This crate *is* the public API, so it opts
// back in.
#![warn(clippy::must_use_candidate)]

mod arrow_inserter;
/// Semantic version of this crate, resolved at compile time from `Cargo.toml`.
///
/// Used by downstream tools (notably `hyperdb-mcp`) to surface the library
/// version in their own status output without duplicating the version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod arrow_reader;
mod arrow_result;
mod async_arrow_inserter;
mod async_connection;
mod async_connection_builder;
mod async_inserter;
mod async_kv_store;
mod async_prepared;
mod async_result;
mod async_transaction;
mod async_transport;
mod catalog;
mod connection;
mod connection_builder;
pub mod copy;
mod data_format;
mod error;
mod inserter;
mod kv_store;
mod names;
mod params;
pub mod pool;
mod prepared;
mod process;
mod query_as;
mod query_result;
pub(crate) mod query_stats;
mod result;
mod row_accessor;
mod server_version;
mod table;
mod table_copy;
mod table_definition;
mod transaction;
mod transport;

mod grpc_connection;
#[cfg(kani)]
mod proofs;

// Third-party crates whose types appear in this crate's public API. Re-exported
// so callers use exactly the version we are built against; a major bump of any
// of them is a `hyperdb-api` major (see README, "Semver and re-exported crates").
pub use arrow;
pub use chrono;
pub use geo_types;

pub use arrow_inserter::ArrowInserter;
pub use arrow_reader::ArrowReader;
pub use arrow_result::{
    ArrowChunk, ArrowRow, ArrowRowset, ChunkSource, FromArrowValue, parse_arrow_ipc,
};
pub use async_arrow_inserter::{AsyncArrowInserter, AsyncArrowInserterOwned};
pub use async_connection::AsyncConnection;
pub use async_connection_builder::AsyncConnectionBuilder;
pub use async_inserter::AsyncInserter;
pub use async_kv_store::AsyncKvStore;
pub use async_prepared::{AsyncPreparedStatement, AsyncPreparedStatementOwned};
pub use async_result::AsyncRowset;
pub use async_transaction::AsyncTransaction;
pub use catalog::Catalog;
pub use connection::{Connection, CreateMode, ScalarValue};
pub use connection_builder::ConnectionBuilder;
pub use error::{ColumnErrorKind, Error, Result};
pub use params::{ParamFormat, ToSqlParam};
pub use prepared::PreparedStatement;
// TLS settings for TCP connections (`ConnectionBuilder::tls` and friends).
pub use hyperdb_api_core::client::tls::{ParseTlsModeError, TlsConfig, TlsMode};
// Re-export Notice for callback registrants. `hyperdb-api-core`'s
// `client::Error` is intentionally NOT re-exported — callers match
// directly on the flat `Error` enum this crate defines.
pub use hyperdb_api_core::client::{Notice, NoticeReceiver};
pub use inserter::{ChunkSender, ColumnMapping, InsertChunk, Inserter, IntoValue, MappedInserter};
pub use kv_store::{BatchGuardOutcome, BatchSetOutcome, KvStore, SetOutcome};
pub use names::{
    DatabaseName, Name, SchemaName, TableName, escape_name, escape_sql_path, escape_string_literal,
};
pub use process::{HyperProcess, ListenMode, Parameters, TransportMode};
pub use query_stats::{LogFileStatsProvider, QueryStats, QueryStatsProvider};
pub use result::{FromRow, ResultColumn, ResultSchema, Row, RowIterator, RowValue, Rowset};
pub use row_accessor::RowAccessor;

// NOTE: proc-macro re-exports (FromRow, Table, query_as!) are intentionally
// absent from hyperdb-api. Re-exporting them creates a dependency cycle:
//   hyperdb-api → hyperdb-api-derive → hyperdb-compile-check → hyperdb-api
// Users add `hyperdb-api-derive` directly with the features they need.
pub use query_as::{QueryAs, QueryScalar};
pub use server_version::ServerVersion;
pub use table::Table;
pub use table_copy::{CopyTableReport, UnpreservedItem, UnpreservedReason};
pub use table_definition::{ColumnDefinition, Persistence, TableConstraint, TableDefinition};
pub use transaction::Transaction;

// Re-export types from hyperdb-api-core's types layer.
pub use hyperdb_api_core::types::{
    Date, Geography, Interval, Nullability, Numeric, OffsetTimestamp, Oid, SqlType, Time,
    Timestamp, Type,
};

/// Error type for geography conversions (WKT parsing, WKB encoding and decoding).
pub use hyperdb_api_core::types::GeoError;

/// Re-export of the PostgreSQL OID constants, e.g. `hyperdb_api::oids::INT`.
pub use hyperdb_api_core::types::oids;

// Re-export gRPC types (always available)
pub mod grpc {
    //! gRPC transport types for Hyper database access.
    //!
    //! This module provides two ways to use gRPC:
    //!
    //! 1. **Unified Connection** (recommended): Use `Connection::connect()` with an
    //!    `https://` or `http://` URL - transport is auto-detected.
    //!
    //! 2. **Direct gRPC**: Use `GrpcConnection` or `GrpcConnectionAsync` for
    //!    explicit gRPC access with full control over transfer modes and async.
    //!
    //! # Transfer Modes
    //!
    //! - `TransferMode::Sync` - All results in one response (simple, 100s timeout)
    //! - `TransferMode::Async` - Header only, fetch results via `GetQueryResult`
    //! - `TransferMode::Adaptive` - First chunk inline, rest streamed (default, recommended)

    // Re-export connection types from grpc_connection module
    pub use crate::grpc_connection::{GrpcConnection, GrpcConnectionAsync};

    // Configuration and result types. The raw core clients (`GrpcClient`,
    // `GrpcClientSync`) and `GrpcError` are deliberately not re-exported: they
    // would put tonic and prost types into this crate's public API. Errors
    // surface as `hyperdb_api::Error`.
    pub use hyperdb_api_core::client::grpc::{
        GrpcColumnInfo, GrpcConfig, GrpcQueryResult, GrpcResultChunk, TransferMode,
    };
}

/// Macro for creating table definitions with a fluent syntax.
///
/// This macro simplifies the common pattern of creating table definitions
/// with multiple columns by providing a more compact syntax.
///
/// # Syntax
///
/// ```text
/// table! {
///     "table_name" {
///         "column_name": SqlType::type_name(), NULLABLE | NOT_NULL,
///         // ... more columns
///     }
/// }
/// ```
///
/// # Example
///
/// ```no_run
/// # use hyperdb_api::{table, TableDefinition, SqlType, Result};
/// # fn example() -> Result<()> {
/// let orders = table! {
///     "Orders" {
///         "Address ID": SqlType::small_int(), NOT_NULL,
///         "Customer ID": SqlType::text(), NOT_NULL,
///         "Order Date": SqlType::date(), NOT_NULL,
///         "Order ID": SqlType::text(), NOT_NULL,
///         "Ship Date": SqlType::date(), NULLABLE,
///         "Ship Mode": SqlType::text(), NULLABLE,
///     }
/// };
///
/// // Equivalent to:
/// let orders_manual = TableDefinition::new("Orders")
///     .add_required_column("Address ID", SqlType::small_int())
///     .add_required_column("Customer ID", SqlType::text())
///     .add_required_column("Order Date", SqlType::date())
///     .add_required_column("Order ID", SqlType::text())
///     .add_nullable_column("Ship Date", SqlType::date())
///     .add_nullable_column("Ship Mode", SqlType::text());
/// # Ok(())
/// # }
/// ```
#[macro_export]
macro_rules! table {
    // Match table with schema.table syntax
    ($schema:literal.$table:literal {
        $($col_name:literal: $col_type:expr, $nullability:ident),* $(,)?
    }) => {{
        #[allow(unused_mut)]
        let mut table_def = $crate::TableDefinition::new($table).with_schema($schema);
        $(
            table_def = table!(@add_column table_def, $col_name, $col_type, $nullability);
        )*
        table_def
    }};

    // Match simple table name
    ($table:literal {
        $($col_name:literal: $col_type:expr, $nullability:ident),* $(,)?
    }) => {{
        #[allow(unused_mut)]
        let mut table_def = $crate::TableDefinition::new($table);
        $(
            table_def = table!(@add_column table_def, $col_name, $col_type, $nullability);
        )*
        table_def
    }};

    // Helper to add column based on nullability
    (@add_column $table_def:expr, $col_name:literal, $col_type:expr, NULLABLE) => {
        $table_def.add_nullable_column($col_name, $col_type)
    };
    (@add_column $table_def:expr, $col_name:literal, $col_type:expr, NOT_NULL) => {
        $table_def.add_required_column($col_name, $col_type)
    };
}
