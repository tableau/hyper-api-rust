# Changelog

All notable changes to the `hyperdb-api-core` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

> **Note:** `hyperdb-api-core` is internal implementation detail for
> [`hyperdb-api`](https://crates.io/crates/hyperdb-api). It is published to
> crates.io for dependency resolution only. Items exposed here may change
> between any two releases, including patch releases, without semver
> deprecation. **Use `hyperdb-api` directly.**

## [Unreleased]

## [0.1.1] - 2026-05-13

### Added

`types` module — SQL type system, binary encoding, OIDs:

- `SqlType`, `Type`, `Nullability` for SQL type metadata
- `ColumnDefinition` for column-level schema
- `Oid` and the `oids` constants module for PostgreSQL OID handling
- `Date`, `Time`, `Timestamp`, `OffsetTimestamp`, `Interval` temporal types with chrono interop
- `Geography` and `GeoError` for geographic type support (WKT/WKB with `geo-types`)
- `Numeric` for arbitrary-precision decimal
- `FromHyperBinary`, `ToHyperBinary`, `IsNull` traits for binary wire encoding
- `ChronoConversionError` for chrono interop failures
- `bytes` re-exported for downstream convenience

`protocol` module — PostgreSQL wire protocol and HyperBinary COPY:

- `copy` submodule for HyperBinary COPY format helpers
- `escape` submodule for SQL identifier and literal escaping
- `message` submodule for PostgreSQL wire-protocol message framing
- `types` submodule with `ParseError` for protocol-level type parsing

`client` module — sync/async TCP and gRPC clients:

- Sync clients: `Client`, `CopyInWriter`, `QueryStream`, `PreparedStatement`, `OwnedPreparedStatement`, `PreparedQueryStream`, `SyncStream`, `SqlParam`
- Async clients: `AsyncClient`, `AsyncCopyInWriter`, `AsyncCopyInWriterOwned`, `AsyncRawConnection`, `AsyncPreparedStatement`, `AsyncPreparedQueryStream`, `AsyncStream`, `AsyncQueryStream`
- gRPC clients: `GrpcClient`, `GrpcConfig`, `GrpcError`, `GrpcQueryResult`, `GrpcResultChunk` (in the `grpc` submodule)
- Connection plumbing: `Config`, `ConnectionEndpoint`, `Cancellable`
- Error types: `Error`, `ErrorKind`, `Result`
- Notices: `Notice`, `NoticeReceiver`
- Result-set primitives: `Row`, `BatchRow`, `StreamRow`, `FromBinaryValue`
- Statement metadata: `Column`, `ColumnFormat`
- Submodules: `auth` (cleartext / MD5 / SCRAM-SHA-256), `tls`

Crate-level:

- Re-exports of `protocol` and `types` from the `client` module for convenience
- Optional `salesforce-auth` feature for Salesforce Data Cloud OAuth (used by the companion `hyperdb-api-salesforce` crate)
