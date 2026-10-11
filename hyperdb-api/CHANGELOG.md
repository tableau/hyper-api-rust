# Changelog

All notable changes to the `hyperdb-api` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- **BREAKING:** the `arrow` dependency moved from **59** to **60**. Arrow types
  appear in this crate's public API, so consumers must move to `arrow` 60 in
  lockstep. No source change was needed on our side. `parquet` (used by
  `hyperdb-mcp`) moved to 60 with it.
- `hyperdb-api` now re-exports `arrow`, `chrono` and `geo_types` from the crate
  root, and the README states the policy: a major bump of a re-exported crate
  is a `hyperdb-api` major.
- **Breaking:** `PreparedStatement::query` and `AsyncPreparedStatement::query`
  now return a rowset borrowing the *statement* (`Rowset<'stmt>` /
  `AsyncRowset<'stmt>`), not just the connection. Previously a rowset could
  outlive its statement, and dropping the statement then deadlocked on the
  connection lock the rowset still held. The compiler now rejects that code.
- **BREAKING: `SqlType` is `#[non_exhaustive]`, and so are the struct variants
  of `SqlType` (`Numeric`, `Varchar`, `Char`) and of `Error` (`Connection`,
  `Server`, `Closed`, `Cancelled`, `Column`, `ColumnIndexOutOfBounds`,
  `Internal`) and `ColumnErrorKind::TypeMismatch`.** Match `SqlType` with a
  wildcard arm, end any pattern that names variant fields with `..`, and build
  these values with the constructors (`SqlType::numeric`, `Error::server`, ...)
  instead of struct expressions. This lets a minor release add a type or a
  field without a major bump.
- **Arrow `Decimal128` / `Decimal256` columns whose precision is outside
  `1..=38` (or whose scale exceeds the precision) now map to `SqlType::Text`**
  rather than an invalid `NUMERIC`, like other unmapped Arrow types.
- **`QueryAs::new` and `QueryScalar::new` now take `&[&dyn ToSqlParam]`**
  instead of `&[&dyn Debug]`. These constructors are emitted by the
  `query_as!` / `query_scalar!` macros and are not meant to be called
  directly, but code that did call them must now pass `ToSqlParam` values.
- **BREAKING: `hyperdb_api::grpc` no longer re-exports `GrpcClient`,
  `GrpcClientSync` or `GrpcError`.** They are the raw `hyperdb-api-core`
  clients and carry `tonic` / `prost` types, which the core crate declares
  unstable. Use `GrpcConnection` / `GrpcConnectionAsync` (or the unified
  `Connection`); failures surface as `hyperdb_api::Error`. The module now also
  exports `GrpcColumnInfo`, which `GrpcQueryResult::columns` returns.
- **BREAKING: escape hatches that returned `hyperdb-api-core` types are no
  longer public.** `Connection::tcp_client`, `AsyncConnection::async_tcp_client`,
  `AsyncConnection::from_async_client`, `HyperProcess::connection_endpoint`,
  `ColumnDefinition::to_types_column_definition` and
  `impl From<hyperdb_api_core::types::ColumnDefinition> for ColumnDefinition`
  are removed from the public API. Replacements:
  `HyperProcess::connection_endpoint_string()` (the endpoint
  `Connection::new` connects to, as a `String`) and
  `AsyncConnection::copy_in_hyperbinary` (below), which replaces hand-written
  COPY loops over `async_tcp_client`.
- **BREAKING: the async pool no longer exposes `deadpool` types.**
  `pool::Pool` and `pool::PooledConnection` were aliases of
  `deadpool::managed::Pool` / `Object`; they are now `hyperdb-api` types, and
  `pool::ConnectionManager` is crate-private. `Pool::get` returns
  `hyperdb_api::Result<PooledConnection>` — a wait or create timeout is
  `Error::Timeout`, a closed pool is `Error::InvalidOperation`, and a failed
  connect is the underlying error — so the
  `.map_err(|e| Error::internal(e.to_string()))` adapter is no longer needed.
  A connection that fails its recycle check (or whose recycle times out) is
  discarded and replaced, never reported to the caller. `Pool::status` returns
  `PoolStatus` (as `ConnectionPool` does), and `PooledConnection::take` frees
  the slot. The `deadpool` version is no longer part of this crate's semver
  surface. The deadpool-only API is gone with it: `Pool::resize`, `retain`,
  `timeout_get`, `manager` and `weak`, `status().waiting`, and
  `Object::id`, `metrics`, `AsRef` and `AsMut`.
- **BREAKING: `hyperdb_api::grpc::TransferMode` is now a `hyperdb-api-core`
  enum (`Sync`, `Async`, `Adaptive`)** instead of the prost-generated type, so
  no protobuf type appears in `GrpcConfig::transfer_mode` or
  `ConnectionBuilder::transfer_mode`. `Adaptive` stays the default; the
  protocol's `Unspecified` value is no longer constructible.
- **BREAKING: `GrpcColumnInfo` no longer has public fields.** Use
  `GrpcColumnInfo::name()` and `type_name()`.
- **`HyperProcess` makes a caller-supplied `domain_socket_directory`
  absolute.** A relative directory produced an endpoint string such as
  `rs-123/hyper` that `Connection::new` could not resolve; the endpoint now
  always starts with `/` on Unix.

### Added

- **`AsyncConnection::copy_in_hyperbinary`** streams a pre-encoded
  `HyperBinary` buffer (for example from `InsertChunk`) into a table with
  `COPY ... FROM STDIN`, slicing it under `hyperd`'s COPY packet cap, and
  returns the inserted row count. The buffer must start with exactly one
  HyperBinary header (the first `InsertChunk::take()` of a fresh or cleared
  chunk); a headerless buffer is rejected with `Error::InvalidOperation`. The
  future is not cancel-safe: dropping it mid-write leaves the connection
  reporting unhealthy, so a pool evicts it on the next checkout.
- **`HyperProcess::connection_endpoint_string`**, and `Pool::close` /
  `Pool::is_closed` on the async pool.
- **TLS for TCP connections.** Until now every TCP connection was plaintext,
  whatever the server offered. `ConnectionBuilder::tls` and
  `AsyncConnectionBuilder::tls` take a `TlsMode` or a `TlsConfig` (both
  re-exported from the crate root) with libpq `sslmode` semantics: `Disable`
  (the default), `Prefer`, `Require`, `VerifyCa` and `VerifyFull`, plus a root
  certificate, a client certificate for mutual TLS, and a server-name
  override. `Prefer` falls back to plaintext only when the server declines
  TLS, never after a failed handshake or verification. A query cancel for a
  TLS session is sent over TLS. `Connection::is_tls` and
  `AsyncConnection::is_tls` report whether the session is encrypted.
  - Over a Unix domain socket or a named pipe, `Prefer` connects in plaintext
    and the modes that require TLS fail with `Error::FeatureNotSupported`. A
    gRPC connection picks TLS through its `https://` scheme and rejects
    `tls()` the same way.
  - The `Connection::connect*` shortcuts and `Connection::new` stay
    plaintext.
  - **BREAKING:** `PoolConfig` and `SyncPoolConfig` gain a public `tls` field
    and a `tls()` builder, applied to every connection the pool opens, so a
    struct-literal `PoolConfig { .. }` no longer compiles. The async pool now
    opens connections through `AsyncConnectionBuilder`: a malformed port in
    its endpoint is an `Error::Config` instead of silently becoming 7483, and
    an authenticated pool on a Unix-socket or named-pipe endpoint connects
    there instead of misreading the path as a TCP host. On a gRPC endpoint a
    `create_mode` other than `DoNotCreate` now fails `Pool::get` with
    `Error::FeatureNotSupported` instead of being ignored, as it already did
    for the sync pool.
  - TLS session resumption is disabled: `hyperd` requests client certificates
    without an OpenSSL session ID context, so it aborts every resumed
    handshake (and a cancel would resume).

### Fixed

- **Reserved-word table and column names** (`order`, `user`, `table`, ...) no
  longer break generated SQL. `TableDefinition::qualified_name`, `table_name`,
  `schema_name`, `database_name`, `to_drop_sql` and the `MappedInserter`
  staging statements now always double-quote identifiers, so every inserter's
  `COPY` works on such tables. The generated SQL text changes (`users` becomes
  `"users"`); the identifiers resolved are the same.

- **`query_as!` / `query_scalar!` silently dropped their `$N` bind
  arguments.** The arguments were stringified and never sent, so any query
  with a placeholder failed at the server with "expected to have 0
  parameter(s)". They are now encoded and bound through the same path as
  `Connection::fetch_all_as_params`.
- **`ConnectionBuilder::query_timeout` and
  `AsyncConnectionBuilder::query_timeout` were recorded and never applied.**
  The timeout is now set as the session's `query_timeout` right after
  connecting, so `hyperd` cancels a statement that runs longer (SQLSTATE
  57014, "canceled"). A zero timeout and a timeout on a gRPC connection are
  rejected at `build()` instead of being ignored.
- **`AsyncConnection::without_database` and `AsyncConnection::connect_with_auth`
  could not reach a Unix domain socket or named pipe.** They parsed every
  endpoint as `host:port`, so a socket path failed with "failed to lookup
  address information". Like their sync counterparts, they now go through
  `AsyncConnectionBuilder`, which also makes a malformed port an
  `Error::Config` instead of a silent fallback to port 7483, and lets
  `without_database` connect to a gRPC endpoint.

- **`Date`, `Timestamp` and `OffsetTimestamp` query parameters were bound with
  the wrong epoch.** `ToSqlParam::encode_param` sent absolute Julian-based
  values, but Hyper's binary `Bind` expects PostgreSQL-epoch values (days /
  microseconds since 2000-01-01) and applies the Julian offset itself, so a
  bound `2024-01-15` came back as `8736-02-22`. The parameters now send
  PG-epoch values. The `Inserter` (HyperBinary COPY) path was unaffected.
- **`Inserter::add_numeric` and `AsyncInserter::add_numeric` ignored the
  value's own scale.** The unscaled integer was written as-is, so
  `Numeric::new(12345, 1)` (1234.5) inserted into a `NUMERIC(10, 2)` column was
  stored as `123.45` — a silent 10× magnitude error. The value is now rescaled
  to the column's declared scale; a value that would lose non-zero digits or
  overflow is rejected with `Error::Conversion` instead of being stored wrong.
- **`HyperProcess` no longer deletes a caller-supplied socket directory on
  drop.** Drop removed any IPC socket directory whose basename started with
  `hyper-`, so a `domain_socket_directory` such as `~/hyper-data` was wiped with
  everything in it. Only the temp directory the process created itself is
  removed now.

## [1.0.0-rc.4] - 2026-09-08

### Fixed

- **IPC transport: a process that spawns several IPC `HyperProcess` instances
  now gives each a distinct endpoint on both platforms.** The Windows named-pipe
  name and the default Unix domain-socket *directory* were both keyed only on
  `std::process::id()` (`hyper-<pid>`), so a second instance in the same process
  — the MCP daemon restarting `hyperd`, or a test harness driving several in
  turn — reused the endpoint. Sequentially this worked only because `Drop`
  removed the prior artifact first; two concurrently-live instances collided and
  the second surfaced as a 60-second callback-listener timeout. Both the pipe
  name and the default socket directory now carry a monotonic per-process suffix
  (`hyper-<pid>-<seq>`), so concurrent IPC instances no longer collide. A
  caller-supplied `domain_socket_directory` is used verbatim and is the caller's
  responsibility to keep unique.

## [1.0.0-rc.3] - 2026-09-07

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

- **Names longer than 63 characters are no longer rejected.** `escape_name`,
  `Name`, `SchemaName`, `DatabaseName` and `TableName` enforced PostgreSQL's
  63-character `NAMEDATALEN`, which Hyper does not share — it stores a
  283-character table name without complaint. The typed name API therefore
  refused names the engine had already written, making such tables
  unaddressable through it even though a raw query could reach them. The bound
  is now a 1024-character sanity limit; SQL-injection safety comes from
  quoting, not from the length check.
- `TableDefinition::to_create_sql` emitted column, collation and constraint
  identifiers unquoted when they were legal bare identifiers. The underlying
  check does not know the SQL reserved word list, so an all-lowercase keyword
  — a column named `select` or `order` — was emitted bare and the engine
  rejected the statement. Generated DDL now quotes identifiers
  unconditionally, which is semantically identical for every other name.
- **A `Numeric` with `scale() > 0` can now be bound to a parameterized query.**
  Previously every parameter travelled as PostgreSQL binary, and Hyper rejects
  a binary `NUMERIC` whose `dscale` exceeds the parameter's resolved scale with
  SQLSTATE `0A000` ("cannot handle truncation when reading numerics") — and a
  declared `numeric` parameter OID carries no type modifier, which Hyper
  resolves to `NUMERIC(1,0)`, so *every* scaled value tripped that check
  regardless of the SQL. Scaled values now bind as text with the OID left
  unspecified, letting the server infer the scale from context; whole numbers
  (`scale() == 0`) keep the binary fast path and the concrete `NUMERIC` OID
  unchanged. One consequence to know about: because a scaled value has no
  declared OID, a bare `SELECT $1` returns it as `TEXT` — wrap it in
  `CAST($1 AS NUMERIC(p,s))` when the result type matters. Two further
  consequences of that inference, now documented on `ToSqlParam for Numeric`:
  `Row::get::<Numeric>` returns `None` for such a column (it really is text,
  and `get::<String>` yields `"1234.56"`), and in `WHERE textcol = $1` the
  comparison resolves to a *string* comparison, so `1234.56` matches
  `'1234.56'` but not `'1234.560'`.

  **This applies to the one-shot `query_params` / `command_params` path
  only.** A `PreparedStatement` fixes its parameter OIDs at `prepare_typed`
  time, before any value exists, so a single prepared statement cannot accept
  both scale classes: `oids::NUMERIC` rejects scaled values with `22003`, and
  `Oid::new(0)` rejects whole numbers with `0A000`. Use `query_params` when
  the scale varies across calls. `Geography` has no such split — it declares a
  concrete OID and always binds as text, so it works on both paths.

- **Binding a `Numeric` whose `scale` is 39 or more no longer panics.** The
  new text path renders the value through `Display`, which divided by
  `10u128.pow(scale)` and overflowed. `Numeric::new` is an unvalidated
  `const fn` and `try_from_f64` validates only the value against the 38-digit
  precision limit, so `Numeric::from_f64(1e-39, 39)` reached it. Such a value
  is out of range for the engine either way, but it now travels to the server
  and is rejected cleanly instead of panicking inside the client.

- `Geography::to_sql_literal` no longer renders a Hyper-legacy value as
  `NULL`. Substituting `NULL` for a value that merely cannot be converted to
  WKT silently erases it; the literal now carries the lossily-decoded bytes so
  the server rejects it with `22P02`, matching what `encode_param` already did
  for the same value. Single quotes are escaped in both branches, so neither
  can break out of the literal.
- The `HYPERD_PATH is not set` error suggested
  `cargo run -p hyperd-bootstrap -- download`, a package that has not existed
  since the rename to `hyperdb-bootstrap`. The command failed with
  "package(s) `hyperd-bootstrap` not found in workspace", which is the first
  thing a new user saw when `HYPERD_PATH` was unset.
- The benchmark examples reported **MiB/s under an `MB/s` label**. `fmt_mb`,
  `BenchRecord::mb_per_sec` and the `memory_*_mb` helpers in
  `benches/common.rs`, plus the equivalent divisions in `benchmark`,
  `arrow_batching_benchmark` and `async_parallel_benchmark`, all divided byte
  counts by 1024². Their sibling formatters (`fmt_count`, `fmt_rate`,
  `fmt_size`) were already decimal, so a single `MB/sec` column in
  [docs/BENCHMARK_GUIDE.md](../docs/BENCHMARK_GUIDE.md) carried two different
  units depending on which platform produced the row — a 4.86% discrepancy.
  All `MB`-labelled output is now decimal (10^6). Installed-RAM reporting stays
  binary, since RAM is conventionally quoted that way.
- **The async pool's default recycle probe now discharges a transaction
  left open by a panicked or cancelled task.** Rust has no async `Drop`, so
  `AsyncTransaction::drop` cannot issue a `ROLLBACK` when dropped without an
  explicit `commit()`/`rollback()` — it only warns, leaving the transaction
  open on the connection. `ConnectionManager::recycle`'s default
  `RecycleStrategy::SelectOne` probe previously ran `SELECT 1`, which
  succeeds even inside an open transaction (confirmed against the real
  engine) and so never detected the leak; the next checkout silently
  inherited a connection mid-transaction. `SelectOne` now issues an
  unconditional `ROLLBACK` instead — a no-op when nothing is open, confirmed
  empirically — at the same one-round-trip cost as the probe it replaces.
  `RecycleStrategy::Ping`, `::None` and `::Custom` are unchanged and do not
  discharge a leaked transaction; see their doc comments. The sync pool's
  `SyncRecycleStrategy::SelectOne` is unaffected — `Transaction`'s `Drop` can
  and does roll back synchronously, so the sync pool never leaks one of
  these. Fixes [issue #263](https://github.com/tableau/hyper-api-rust/issues/263).
- **Corrected the documented behavior of `RecycleStrategy::None`.** Its doc
  comment claimed the pool "still drops connections that fail the passive
  `AsyncConnection::is_alive` check". The async connection manager never calls
  `is_alive` — `None` is a genuine no-op, so a connection is handed out in
  whatever state the previous borrower left it, and a dead one surfaces on
  first use. (Only the *sync* pool performs an `is_alive` check.) Behavior is
  unchanged; the documentation was wrong, and misleadingly reassuring for the
  caller most exposed to it — one combining `None` with `AsyncTransaction`.

### Changed

- `Transaction`'s `Drop` now logs the implicit rollback it was already
  performing: a `tracing::warn!` when the rollback itself fails, and a
  `tracing::debug!` when it succeeds. Behavior is otherwise unchanged (the
  rollback is still best-effort and `Drop` still cannot report failure), but a
  transaction discharged by a panic is no longer invisible in the logs — on
  that path there is no `Err` for a caller to inspect. `AsyncTransaction`'s
  `Drop` already warned.

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

- **BREAKING:** `KvStore::set`, `KvStore::set_as`, and `KvStore::set_batch`
  (plus their `AsyncKvStore` twins) now return `SetOutcome` or
  `BatchSetOutcome` instead of `Result<()>`, reporting whether each write
  created a new key or overwrote an existing one. The `created` signal
  eliminates silent data loss when an LLM accidentally clobbers existing KV
  data. Callers that ignored the `Result` (statement-position `set("k","v")?;`)
  — including `let _ = set(...)?;` — still compile unchanged. The genuinely
  breaking cases are callers that named the unit return (`let x: () =
  set(...)?;`) or that returned `set(...)` where a `Result<()>` was expected;
  these now see `SetOutcome`/`BatchSetOutcome` and must adapt.

### Added

- `Catalog::copy_table` — copies a table while reproducing its schema instead
  of letting `CREATE TABLE AS SELECT` infer it. CTAS derives the destination
  schema from the query's result columns, which carry types but no
  constraints, so a CTAS "copy" came out with every column nullable, no
  defaults and no keys. `copy_table` reflects the source out of `pg_catalog`,
  issues an explicit `CREATE TABLE`, then moves rows with `INSERT ... SELECT`.
  Source and destination may live in different databases. Returns a
  `CopyTableReport` — **a successful return does not imply full fidelity**;
  check `CopyTableReport::is_fully_preserved`.
- `CopyTableReport`, `UnpreservedItem` and `UnpreservedReason` — what a copy
  reproduced (`NOT NULL`, `DEFAULT`, `COLLATE`, `ASSUMED PRIMARY KEY`,
  `ASSUMED UNIQUE` counts) and what it deliberately dropped. Each
  `UnpreservedItem` names the `table` it came from, so a whole-database copy —
  which merges one report per table — stays actionable rather than reporting a
  bare column name. Hyper stores non-literal defaults
  database-qualified (`NOW()` reads back as `"mydb"."pg_catalog"."now"()`), so
  copying one verbatim into another database would leave the copy depending on
  `"mydb"` still being attached. Those defaults are dropped and reported rather
  than reproduced unsoundly.
- `TableConstraint` — `AssumedPrimaryKey` / `AssumedUnique`, the only
  table-level constraint forms Hyper accepts. Real `PRIMARY KEY`, `UNIQUE` and
  `FOREIGN KEY` are rejected at `CREATE TABLE` with `Index support is
  disabled`, and `CHECK` with `check constraints not implemented yet`, so no
  Hyper table can carry one. Assumed constraints are recorded and reported by
  the engine but **not enforced** — a duplicate key insert succeeds.
- `TableDefinition::constraints`, `push_constraint` and `set_constraints`;
  `ColumnDefinition::default_expr`, `set_default_expr` and
  `clear_default_expr`. `TableDefinition::to_create_sql` now emits `DEFAULT`
  clauses and table-level assumed constraints.
- `Catalog::get_table_definition` now also populates `DEFAULT` expressions,
  column collations and assumed key constraints, not just names, types and
  nullability.
- `impl ToSqlParam for Geography` — geography values can now be passed to
  `query_params` / `command_params` and used in predicates and projections.
  They bind as WKT text, because Hyper has no PostgreSQL-binary *input*
  function for `geography` (binding one in binary fails with `42883`). A
  `Geography` read back out of Hyper is in Hyper's proprietary legacy format,
  which has no client-side WKT rendering; binding one of those is rejected by
  the server with `22P02` rather than stored incorrectly. Check
  `Geography::binary_format` first, or build the value with
  `Geography::from_wkt` / `from_wkb`.
- `ToSqlParam::param_format`, returning the new `ParamFormat` (`Text` or
  `Binary`), and a `ParamFormat` re-export at the crate root. The method is
  defaulted to `ParamFormat::Binary`, so existing external `ToSqlParam` impls
  are unaffected. Parameters are bound with a per-parameter format-code array,
  so text-only types travel alongside binary ones in the same statement
  without slowing the binary path.
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
