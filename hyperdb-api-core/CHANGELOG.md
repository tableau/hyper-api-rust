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

### Added

- **`client::tls`: TLS for TCP connections**, negotiated with PostgreSQL's
  `SSLRequest` and libpq `sslmode` semantics. `TlsConfig` / `TlsMode` (with
  `ParseTlsModeError`) are set through `Config::with_tls` and read back with
  `Config::tls`. `Client::is_tls` and `AsyncClient::is_tls` report the
  outcome, and `SyncStream::Tls` / `AsyncStream::Tls` carry the encrypted
  stream. A cancel request for a TLS session is sent over TLS. Session
  resumption is disabled, because `hyperd` aborts every resumed handshake.

### Removed

- **`SyncStream::try_clone`.** A rustls stream cannot be cloned, and nothing
  called it.
- **The old, never-wired `client::tls` surface:** `TlsConfig`'s public fields
  (`verify_server`, `ca_cert_path`, `client_cert_path`, `client_key_path`,
  `server_name`), `TlsConfig::danger_accept_invalid_certs`, the `TlsMode`
  helpers `is_enabled` / `is_required` / `verify_server` / `verify_hostname`,
  and the `rustls_impl` module (`create_connector`, `TlsStream`,
  `wrap_stream`).

### Changed

- **`client::tls` is reshaped around libpq `sslmode`** (BREAKING).
  `TlsConfig::new` takes the `TlsMode`; `ca_cert` is now `root_cert`;
  `TlsMode::VerifyCA` is now `TlsMode::VerifyCa`; `TlsConfig`'s fields are
  private (`TlsConfig::mode` reads the mode back).
- **`client::Error` has a new `Tls` variant** (BREAKING for exhaustive
  matches; the enum is not `#[non_exhaustive]`), built with `Error::tls`.
  Handshake and certificate failures, including a `rustls::Error` surfacing
  through I/O, map to it.
- **`SyncStream` / `AsyncStream` gained a `Tls` variant, and `is_tcp()` is
  `true` for it** (a TLS stream runs over TCP); `is_tls()` tells them apart.
- **`Config`'s `FromStr` error type is `client::Error`** (was `String`). The
  libpq TLS keys (`sslmode`, `sslrootcert`, `sslcert`, `sslkey`, ...) are
  rejected with `Error::Config` instead of being forwarded to the server as
  startup options, where they were silently ignored.
- **`SqlType` and its struct variants (`Numeric`, `Varchar`, `Char`) are
  `#[non_exhaustive]`.** `SqlType::try_numeric` is the non-panicking
  counterpart of `SqlType::numeric` for untrusted precision and scale.
- **`client::grpc::TransferMode` is now a crate-owned enum** (`Sync`, `Async`,
  `Adaptive`) with `From` conversions to the generated protobuf type, replacing
  the re-export of the prost type. `GrpcColumnInfo`'s `name` and `sql_type`
  fields are now `pub(crate)`; use the accessors.

### Fixed

- `protocol::escape::SqlIdentifier`, `format_table_name` and `escape_identifier`
  now always quote identifiers. They previously left lowercase names bare, so
  reserved words such as `order` produced invalid SQL. Output changes from
  `users` to `"users"`.

- **A cancelled COPY write could wedge the async connection.** Dropping a COPY
  write future mid-frame left a partial `CopyData` frame on the wire, so the
  queued `CopyFail` was read as part of that frame and
  `drain_pending_copy_cancel` waited forever for `ReadyForQuery`.
  `AsyncRawConnection` now tracks an in-flight write: after an interrupted
  write the connection reports unhealthy, no `CopyFail` is queued, and
  `finish_copy` / `cancel_copy` refuse to run on it. The post-cancel drain is
  also bounded and marks the connection desynchronized instead of looping.

- **`Interval` `Display` kept the sign only when the time component was an hour
  or more.** A negative interval under one hour (for example −30 minutes)
  rendered as a positive `00:30:00`; it now renders `-00:30:00`.
- **`AuthenticatedGrpcClient::has_table` returned `true` for every table.** It
  judged existence by the byte length of the Arrow stream, which is non-empty
  even for a zero-row result; it now counts rows.
- **`AuthenticatedGrpcClient::get_table_labels` / `get_column_labels` escape
  their `schema` and `table` arguments** instead of interpolating them into
  the catalog query unescaped.
- **gRPC `QueryParameters::json_positional` and `json_named` produced JSON
  that `hyperd` rejects** (`22023 invalid JSON query parameters`). They sent
  bare values (`[42,"hi"]`) and an object (`{"id":42}`). `hyperd` requires an
  array of typed entries (`[{"type":"bigint","value":"42"}]`, plus `"name"` for
  named parameters), which both now emit. The Hyper type is inferred from the
  JSON value (`bool`, `bigint`, `numeric` above `i64::MAX`, `float8`,
  `varchar`; `null` as a nullable `varchar`); array and object values return
  an error. The docs for `from_json_string` / `from_json_value`, which pass
  JSON through unchanged, now describe the typed format instead of showing
  examples `hyperd` would reject.

## [1.0.0-rc.3] - 2026-09-07

### Added

- `ParamFormat` (`Text` / `Binary`) and a set of `*_with_formats` execution
  methods that take a per-parameter format-code array: `Client::{execute_streaming,
  execute_no_result}_with_formats`, `AsyncClient::{execute_prepared_streaming,
  execute_prepared_no_result}_with_formats`,
  `RawConnection::start_execute_prepared_with_formats`,
  `AsyncRawConnection::{start_execute_prepared, execute_prepared_no_result}_with_formats`,
  and `prepare::execute_prepared_no_result_with_formats`. Hyper accepts a mixed
  format-code array, so a statement can bind the types with no PG-binary input
  function (scaled `NUMERIC`, `geography`) as text while every other parameter
  stays binary. The existing methods are unchanged and still bind everything as
  binary.

### Changed

- **BREAKING:** `client::Error` is now a flat enum — one
  variant per failure mode, matched directly — rather than a struct with a
  `kind` discriminator and a `Box<dyn StdError>` cause channel
  ([#75](https://github.com/tableau/hyper-api-rust/issues/75)). This applies
  the shape `hyperdb_api::Error` already adopted in
  [#70](https://github.com/tableau/hyper-api-rust/issues/70) per the
  [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/)
  (M-ERRORS-CANONICAL-STRUCTS, M-ERRORS-AVOID-WRAPPING-AND-AS-DYN).

  **No effect on `hyperdb-api`'s public API** — `client::Error` and
  `client::ErrorKind` were never re-exported from it, and this crate is
  documented above as internal.

  - `client::ErrorKind` is **removed**, along with `Error::kind()`,
    `Error::new()`, `Error::with_cause()`, and `Error::new_with_details()`.
  - Every variant has a snake_case constructor taking `impl Into<String>`:
    `connection`, `authentication`, `query`, `protocol`, `io`, `config`,
    `timeout`, `cancelled`, `closed`, `conversion`, `feature_not_supported`,
    `other`. `Error::closed()` and `Error::timeout()` previously took no
    arguments and supplied a canned message; they now take one.
  - `Error::io(err: io::Error)` became `Error::from_io(err: io::Error)`, so
    the name `io` could be the message-taking constructor like its peers.
    `impl From<io::Error>` is unchanged.
  - `Connection`, `Cancelled`, and `Closed` carry `{ message, sqlstate }`;
    `Query` carries `{ message, sqlstate, detail, hint }`. Those are exactly
    the variants that could hold a SQLSTATE before, so `sqlstate()`,
    `detail()`, `hint()`, and `message()` return what they used to **on
    those variants**.
  - The enum is deliberately **not** `#[non_exhaustive]`, unlike the public
    `hyperdb_api::Error`. This type is internal with a single in-workspace
    consumer, so it gains nothing from forward-compatible matching, while
    staying exhaustive keeps the compile-time check on
    `From<client::Error> for hyperdb_api::Error` — a new variant must be
    given a deliberate public mapping instead of silently degrading to
    `Error::Internal`.
  - **Narrower diagnostics on four variants.** `Authentication`,
    `FeatureNotSupported`, `Timeout`, and `Other` are single-string, so the
    gRPC error path folds any server `detail` into the message and
    **discards `hint` and `sqlstate`** on those four; the old
    `new_with_details` stored all three regardless of kind. No caller
    observes the loss — the public `hyperdb_api::Error` mapping already
    discarded `hint` and `sqlstate` on the arms these feed — but the stored
    data is genuinely narrower, not merely reshaped.
  - `Error::with_cause` had no call sites, so dropping the `Box<dyn>` channel
    loses no information. `source()` now returns `None` for I/O errors; the
    public `hyperdb_api::Error` mapping already discarded that cause.
- The all-binary `Bind` path now sends a **single** parameter format code
  rather than one per parameter. The PostgreSQL protocol broadcasts a lone
  format code across every parameter, so this is wire-compatible and drops the
  per-execute `Vec<i16>` allocation from the hot path.

- **An empty `param_formats` slice now means "every parameter is binary" on
  every `*_with_formats` method, and a length mismatch is an error.**
  `RawConnection::start_execute_prepared_with_formats` and its async twin
  previously forwarded an empty slice to `Bind` unchanged, and the PostgreSQL
  protocol reads a zero-length format array as *all text* — the opposite of
  what the sibling methods and every in-crate caller meant by `&[]`. Both
  meanings now resolve in one place, which also rejects a non-empty slice
  whose length differs from the parameter count instead of silently
  broadcasting it. The zero-parameter case still sends a zero-length array,
  which is the only context where that is correct.

- **BREAKING:** the optional `arrow` dependency moved from **58** to **59**,
  matching `hyperdb-api`. Only relevant with the `salesforce-auth` feature.

- **BREAKING:** the minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`. The previous 1.81 was
  not achievable in practice — the lockfile already required 1.88 for several
  direct dependencies.

### Fixed

- An `Error` built from an `io::Error` no longer renders its message twice.
  `Error::io` stored the same text as both `message` and `cause`, and
  `Display` printed both, so an I/O failure surfaced as `"refused: refused"`.
  Dropping the cause channel removed the duplication.

  This **changes the `Display` text** of I/O-origin connection errors, and
  those reach the public `hyperdb_api::Error` (its `Connection` variant is
  built from this message). The new text is the intended one, but anything
  matching on the error *string* rather than the variant will see
  `"refused"` where it saw `"refused: refused"`.

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

- **`Numeric`'s `Display` no longer panics for a `scale` of 39 or more.** It
  divided the value by `10u128.pow(scale)`; that divisor overflows `u128` at
  scale 39 (a panic in debug, a garbage divisor in release) and wraps to
  exactly zero at scale 128, dividing by zero. `Numeric::new` is an
  unvalidated `const fn` and `try_from_f64` checks only the value, so such a
  scale is constructible — and it is now reachable at runtime, because
  `hyperdb-api` binds a scaled `NUMERIC` parameter by way of `Display`. The
  decimal point is now inserted into the digit string instead, which is total
  over every `u8` scale; out-of-range values reach the server and are
  rejected cleanly rather than panicking in the client. Rendering is
  unchanged for every scale that previously worked.

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
