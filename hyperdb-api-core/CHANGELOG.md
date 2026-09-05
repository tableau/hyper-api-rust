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

### Changed

- **BREAKING:** the optional `arrow` dependency moved from **58** to **59**,
  matching `hyperdb-api`. Only relevant with the `salesforce-auth` feature.

- **BREAKING:** the minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`. The previous 1.81 was
  not achievable in practice — the lockfile already required 1.88 for several
  direct dependencies.

### Fixed

- **`AuthenticatedGrpcClient::get_table_labels` and `get_column_labels` now
  report Arrow failures instead of returning a partial map.** Both iterated
  record batches with `if let Ok(batch) = batch_result`, so a decode failure
  mid-stream was discarded and the caller received a map covering only the
  batches that happened to decode — indistinguishable from "this table defines
  no labels", which is the worst failure mode for metadata used to render UI.
  A schema mismatch was swallowed the same way, by the `downcast_ref` tuple in
  the same `if let` chain.

  Both now return `Err`. A batch projecting fewer than two columns is also
  reported rather than panicking: `RecordBatch::column(1)` panics out of
  bounds, which the previous code never guarded.

  Callers that prefer the old behavior can keep it explicitly with
  `.unwrap_or_default()`. The parsing itself is unchanged — JSON
  `displayName` extraction, verbatim passthrough for plain descriptions, and
  skipping NULL rows all behave as before, now covered by unit tests.

- `text_from_hyper_binary` and `bytea_from_hyper_binary` no longer risk a
  `usize` overflow on 32-bit targets. Both read a `u32` length prefix, widened
  it to `usize`, and bounds-checked with `buf.len() < 4 + len`; where `usize`
  is 32 bits a declared length near `u32::MAX` wraps that sum to a small value,
  so the check passed and the following slice index panicked. Both now use
  `slice::split_first_chunk::<4>()` and `slice::get`, performing no arithmetic
  on the declared length at all. 32-bit `i686` targets are Tier 1, so this was
  reachable rather than theoretical.

- `Numeric`'s `Display` implementation no longer drops the sign of negative
  values with magnitude less than 1 (the open interval `(-1, 0)`). Values such
  as `-0.5` previously rendered as `0.5000` because the sign was derived from
  the integer part, which is `0` for sub-unit magnitudes. The sign is now
  computed explicitly and the magnitude formatted via `unsigned_abs`, which
  also removes a latent `i128::MIN` overflow panic.

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
