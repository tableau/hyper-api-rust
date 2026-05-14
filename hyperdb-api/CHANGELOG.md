# Changelog

All notable changes to the `hyperdb-api` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
