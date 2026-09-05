# Changelog

All notable changes to the `hyperdb-api` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Removed

- **BREAKING:** `Connection::begin_transaction`, `commit` and `rollback`, and
  the matching `AsyncConnection` methods, are gone. They were `#[deprecated]`
  and `#[doc(hidden)]` since 0.3.0, and 1.0.0 is the boundary at which they can
  actually be dropped. Prefer `Connection::transaction()`, whose RAII guard
  rolls back on drop and cannot leak a half-open transaction across an error
  path. If the guard's `&mut self` borrow is impossible — typically a helper
  that holds `&self` — use the `*_unguarded` methods added below. Migration
  recipe in [docs/TRANSACTIONS.md](../docs/TRANSACTIONS.md#unguarded-transaction-control).

### Fixed

- The `HYPERD_PATH is not set` error suggested
  `cargo run -p hyperd-bootstrap -- download`, a package that has not existed
  since the rename to `hyperdb-bootstrap`. The command failed with
  "package(s) `hyperd-bootstrap` not found in workspace", which is the first
  thing a new user saw when `HYPERD_PATH` was unset.

### Changed

- **BREAKING:** the `arrow` dependency moved from **58** to **59**. Arrow types
  appear in this crate's public API (`ArrowReader`, `ArrowInserter`,
  `AsyncArrowInserter`, `ArrowResult` and the Arrow IPC paths), so a consumer
  must move to `arrow` 59 in lockstep — mixing 58 and 59 in one binary yields
  two incompatible `RecordBatch` types. No source change was needed on our
  side; the workspace builds and its 1568 tests pass unchanged.

  This also **removes the `thrift` dependency**, and with it the Apache Thrift
  "Memory Allocation with Excessive Size Value" advisory. `parquet` 58.x
  required `thrift ^0.17`, which pinned the vulnerable 0.17.0 and could not be
  updated in isolation; `parquet` 59 dropped thrift altogether. That closes an
  advisory this workspace had previously been unable to resolve.

- **BREAKING:** the minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`, so the declared MSRV
  now matches the enterprise consumption path. The previous 1.81 was not
  achievable in practice — the lockfile already required 1.88 for several
  direct dependencies.
- `Connection::stream_as`, `Connection::stream_as_params`,
  `AsyncConnection::stream_as` and `AsyncConnection::stream_as_params` now
  carry an explicit `use<'a, T>` precise-capturing bound on their returned
  `impl Iterator` / `impl Stream`. Edition 2024 makes return-position `impl
  Trait` capture every in-scope lifetime and type parameter by default; the
  explicit capture list pins the previous behaviour rather than widening it.
  Callers are unaffected unless they relied on the opaque type capturing more
  than `'a` and `T`.
- The effective TLS crypto provider is now **ring** rather than AWS-LC. The
  crate already asked for `rustls` with `features = ["ring"]`, but a transitive
  `reqwest` dependency forced the `aws-lc-rs` provider and Cargo's feature
  unification applied it workspace-wide. See the `hyperdb-bootstrap` and
  `hyperdb-api-salesforce` entries for detail.

- **BREAKING:** `KvStore::set`, `KvStore::set_as`, and `KvStore::set_batch` (plus their `AsyncKvStore` twins) now return `SetOutcome` or `BatchSetOutcome` instead of `Result<()>`, reporting whether each write created a new key or overwrote an existing one. The `created` signal eliminates silent data loss when an LLM accidentally clobbers existing KV data. Callers that ignored the `Result` (statement-position `set("k","v")?;`) — including `let _ = set(...)?;` — still compile unchanged. The genuinely breaking cases are callers that named the unit return (`let x: () = set(...)?;`) or that returned `set(...)` where a `Result<()>` was expected; these now see `SetOutcome`/`BatchSetOutcome` and must adapt.

### Added

- `Connection::begin_transaction_unguarded`, `commit_unguarded` and
  `rollback_unguarded`, plus the `AsyncConnection` equivalents. These are the
  supported replacement for the removed deprecated methods and were previously
  `pub(crate)` as `*_raw`. They are not deprecated, but they are not the
  default path either: the caller owns pairing a begin with a commit or
  rollback on **every** path, including panics and cancelled futures, since an
  unmatched begin wedges the session in a way reconnect logic cannot clear.
  Reach for them only when the guard's `&mut self` borrow is impossible.

- `KvStore::set_if_absent` / `AsyncKvStore::set_if_absent` — guarded write that inserts only if the key is absent (no check-then-write race; single `INSERT ... WHERE NOT EXISTS`). Returns `true` if written, `false` if the key already existed (nothing written).
- `KvStore::set_batch_if_absent` / `AsyncKvStore::set_batch_if_absent` — atomic batch variant of `set_if_absent`, returning `BatchGuardOutcome { written, skipped }`. All keys are validated before the transaction opens; an invalid key aborts the whole batch.
- `KvStore::byte_size` / `AsyncKvStore::byte_size` — returns the total byte length of all values in the store (`SUM(OCTET_LENGTH(value))`); 0 for an empty store.
- `KvStore::entries` / `AsyncKvStore::entries` — returns all `(key, value)` pairs sorted by key ascending, materializing the whole store. Intended for small scratchpad stores.
- `SetOutcome`, `BatchSetOutcome`, `BatchGuardOutcome` — public outcome types re-exported from `hyperdb_api` (sync + async twins).
- Key-value store API: `Connection::kv_store` / `AsyncConnection::kv_store` returning
  `KvStore` / `AsyncKvStore` handles over a fixed `_hyperdb_kv_store` table, with
  `get`/`set`/`get_as`/`set_as`/`delete`/`exists`/`size`/`keys`/`pop`/`clear`/`set_batch`,
  plus `kv_list_stores`. Adds the `Error::Serialization` variant.
- `Connection::kv_store_in(database, name)` / `kv_list_stores_in(database)` (plus the
  `AsyncConnection` twins) to open and enumerate KV stores in a specific attached
  database. The database name is identifier-escaped internally.

## [0.1.1] - 2026-05-13

### Added

Connections and process management:

- `Connection` and `AsyncConnection` for sync and async database access
- `ConnectionBuilder` and `AsyncConnectionBuilder` for fluent connection setup
- `HyperProcess` for managing a local `hyperd` server instance
- `Parameters` (with `ListenMode`, `TransportMode`) for `HyperProcess` startup configuration
- `CreateMode` enum for database creation behavior
- `ServerVersion` for querying PostgreSQL-compatible server version

Query execution and results:

- `Rowset` and `AsyncRowset` for streaming query results with constant memory
- `Row`, `RowValue`, `RowIterator`, `ResultColumn`, `ResultSchema` for result-set primitives
- `ScalarValue` for single-value query results
- `FromRow` trait for struct mapping from query rows
- `IntoValue` trait for value conversion
- `query_count` and `fetch_*` convenience methods on `Connection` and `Transaction`

Prepared statements and parameters:

- `PreparedStatement`, `AsyncPreparedStatement`, and `AsyncPreparedStatementOwned` for prepared query execution
- `ToSqlParam` trait and `params::ToSqlParam` machinery for parameterized queries

Transactions:

- `Transaction` and `AsyncTransaction` RAII transaction guards with auto-rollback on drop
- ACID semantics: Atomicity, Consistency, Isolation guaranteed (durability is not provided by this API)

Bulk data insertion:

- `Inserter` and `MappedInserter` for sync row-by-row HyperBinary insertion
- `ArrowInserter` for sync Arrow `RecordBatch` insertion
- `AsyncArrowInserter` and `AsyncArrowInserterOwned` for async Arrow insertion
- `ColumnMapping`, `InsertChunk`, `ChunkSender` for chunked, multi-threaded insertion paths

Reading:

- `ArrowReader`, `ArrowRowset`, `ArrowChunk`, `ArrowRow` for reading query results as Apache Arrow `RecordBatch`es
- `FromArrowValue` and `ChunkSource` traits for Arrow value extraction
- `parse_arrow_ipc` for deserializing raw Arrow IPC bytes into an `ArrowRowset`

Schema and table introspection:

- `Catalog` for schema and table metadata
- `TableDefinition`, `ColumnDefinition`, and `Persistence` for programmatic table-schema creation

Names and SQL escaping:

- `escape_name`, `escape_sql_path`, `escape_string_literal` utilities
- `DatabaseName`, `Name`, `SchemaName`, `TableName` typed name wrappers

Notices, errors, and diagnostics:

- `Error`, `Result<T>`, and `ErrorKind` for top-level error handling
- `Notice` and `NoticeReceiver` for server notice callbacks (warnings, etc.)
- `QueryStats`, `QueryStatsProvider`, and `LogFileStatsProvider` for per-query performance metrics from Hyper's internal log

Modules:

- `copy` module for CSV/TSV import and export via the PostgreSQL COPY protocol
- `pool` module for async connection pooling (deadpool-based)
- `grpc` module for the gRPC transport with Arrow IPC queries (`GrpcConnection`, `GrpcConnectionAsync`, plus re-exports `GrpcClient`, `GrpcClientSync`, `GrpcConfig`, `GrpcError`, `GrpcQueryResult`, `GrpcResultChunk`, `TransferMode`)

Type system (re-exported from `hyperdb-api-core::types`):

- `Date`, `Time`, `Timestamp`, `OffsetTimestamp`, `Interval` temporal types with chrono interop
- `Geography` and `GeoError` for geographic type support (WKT/WKB with `geo-types`)
- `Numeric` for arbitrary-precision decimal
- `oids` constants module
- `SqlType`, `Type`, `Nullability`, `Oid`

Other:

- `VERSION` compile-time crate version constant
- `table!` macro for concise `TableDefinition` construction
- Zero feature flags — all capabilities always available
