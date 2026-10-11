# hyperdb-api-core `client` module: Development Guide

Contributor-facing documentation for the `client` module of the internal `hyperdb-api-core` crate -- the connection layer
between `hyperdb-api` (high-level API) and the `protocol` module (wire protocol).

For user-facing documentation, see the [hyperdb-api README](../../hyperdb-api/README.md).

---

## Architecture Overview

The `client` module provides two transport families with both sync and async variants:

| Transport | Protocol | Capabilities | Variants |
|-----------|----------|-------------|----------|
| **TCP** | PostgreSQL wire protocol (v3) | Full read/write, DDL, COPY | `Client`, `AsyncClient` |
| **gRPC** | Protobuf over HTTP/2 | Read-only (SELECT), Arrow IPC results | `GrpcClient`, `GrpcClientSync` |

### Module Map

```text
src/
  lib.rs                  # Crate root, re-exports
  config.rs               # Config builder for TCP connections
  client.rs               # Sync TCP client (Client, CopyInWriter, QueryStream)
  async_client.rs         # Async TCP client (AsyncClient, AsyncCopyInWriter)
  connection.rs           # RawConnection<S> -- sync wire protocol engine
  async_connection.rs     # AsyncRawConnection<S> -- async wire protocol engine
  auth.rs                 # Authentication: cleartext, MD5, SCRAM-SHA-256
  tls.rs                  # TlsConfig/TlsMode, SSLRequest negotiation, verifiers, TLS cancel
  cancel.rs               # Cancellable trait (transport-agnostic cancel)
  endpoint.rs             # ConnectionEndpoint (TCP, Unix, Named Pipe)
  sync_stream.rs          # SyncStream (TCP/Unix/Pipe wrapper for sync I/O)
  async_stream.rs         # AsyncStream (TCP/Unix/Pipe wrapper for async I/O)
  error.rs                # Error types
  notice.rs               # Server notice/warning handling
  row.rs                  # Row, StreamRow, BatchRow
  statement.rs            # Column metadata, ColumnFormat
  prepare.rs              # Prepared statement support
  grpc/
    mod.rs                # gRPC module root, re-exports
    client.rs             # GrpcClient, GrpcClientSync
    config.rs             # GrpcConfig builder
    executor.rs           # GrpcQueryExecutor state machine
    params.rs             # QueryParameters, ParameterStyle
    result.rs             # GrpcQueryResult, GrpcResultChunk
    error.rs              # gRPC error conversion
    proto.rs              # Generated protobuf types
    authenticated_client.rs  # AuthenticatedGrpcClient (salesforce-auth feature)
tests/
  common/mod.rs           # TestServer helper
  client_tests.rs         # Sync client integration tests
  copy_tests.rs           # COPY protocol tests
  prepared_statement_tests.rs
  async_copy_cancel_tests.rs
```

---

## TCP Transport Internals

### Connection Lifecycle

1. **TCP connect** -- `TcpStream::connect` (or Unix/Named Pipe equivalent)
2. **Startup** -- Send `StartupMessage` with user, database, and protocol version
3. **Authentication** -- Server selects method; client responds (see `auth.rs` module docs)
4. **Parameter exchange** -- Server sends `ParameterStatus` messages (server_version, etc.)
5. **Ready** -- Server sends `ReadyForQuery('I')`, connection is usable

The startup and auth handshake is implemented in `RawConnection::startup()` and
`AsyncRawConnection::startup()`.

### Query Execution Modes

The TCP client supports three query modes. Implementation details (protocol messages,
format codes, memory behavior) are documented in `client.rs` module-level rustdoc.

| Mode | Method | Format | Memory Model |
|------|--------|--------|-------------|
| Simple Query | `query()` | Text | All rows materialized in `Vec<Row>` |
| Extended Query | `query_fast()` | HyperBinary (format code 2, LE binary) | All rows in `Vec<StreamRow>` |
| Streaming | `query_streaming()` | HyperBinary | Chunked via `QueryStream` iterator |

### COPY Protocol

COPY IN uses the PostgreSQL COPY subprotocol:

1. Client sends `Query("COPY ... FROM STDIN WITH (FORMAT ...)")`
2. Server responds with `CopyInResponse`
3. Client sends `CopyData` messages (buffered in `CopyInWriter`)
4. Client sends `CopyDone` (success) or `CopyFail` (abort)
5. Server responds with `CommandComplete` + `ReadyForQuery`

Supported formats: `HYPERBINARY` (default), `ARROWSTREAM`, `CSV`.
The higher-level `hyperdb_api::Inserter` handles binary encoding; `CopyInWriter` is
a transport-level data pump.

### Connection Health and Desynchronization

`RawConnection` tracks a `desynchronized` flag. Once set (e.g., a bounded drain
exceeded its budget before seeing `ReadyForQuery`), all operations fast-fail.
Recovery requires dropping the connection and opening a new one.

Key methods:

- `ensure_healthy()` -- checked before every new request
- `is_healthy()` -- used by pool layers during recycle
- `drain_until_ready_bounded(cap)` -- used by `QueryStream::drop` and error recovery
- `consume_error()` -- drains through `ReadyForQuery` after an `ErrorResponse`

### Cancel Mechanism

PG wire cancellation opens a *separate* TCP connection and sends a 16-byte
`CancelRequest` packet (process ID + secret key). This design allows cancellation
from any thread without acquiring the connection mutex.

See `cancel.rs` for the `Cancellable` trait and `Client::cancel()` for the
user-facing fallible API. `QueryStream::drop` uses `Cancellable` to stop
in-flight streaming queries.

---

## gRPC Transport Internals

### Executor State Machine

`GrpcQueryExecutor` (in `grpc/executor.rs`) drives query execution through a state
machine that handles all three transfer modes. The state transitions and per-mode
paths are documented in that module's rustdoc. Key states:

```text
ReadInitialResults -> RequestStatus -> ReadStatus -> RequestResults -> ReadResults -> Finished
```

SYNC-mode queries go directly from `ReadInitialResults` to `Finished`.
ADAPTIVE-mode queries may take either path depending on result size.

### Cancel via gRPC

Unlike PG wire, gRPC cancels travel as a regular `CancelQuery` RPC multiplexed
over the existing HTTP/2 channel. The cancel carries `x-hyperdb-query-id` metadata
for server-side routing. See `GrpcClient::cancel_query()` rustdoc for the full
design discussion, including why `GrpcClient` does not implement `Cancellable`.

### Protobuf Generation

The `build.rs` script compiles `.proto` files (from the crate-local `protos/`
directory) into Rust types via `tonic-build`. The generated code lives in
`grpc/proto.rs` (included via `include!`).

To regenerate after proto changes:

```bash
cargo build -p hyperdb-api-core
```

---

## Authentication Internals

### Protocol Flow

Authentication is server-directed: the server sends an `AuthenticationRequest`
message indicating which method to use. The client responds accordingly.

The supported methods, their wire formats, and the SCRAM-SHA-256 multi-step
handshake are documented in `auth.rs` module-level rustdoc.

### Key Security Properties

- All intermediate SCRAM key material uses `zeroize::Zeroizing<Vec<u8>>`
- `AuthState` is consumed (moved) by `scram_verify_server`, ensuring keys
  are dropped after verification
- MD5 uses a double-hash scheme: `MD5(MD5(password + user) + salt)`

---

## TLS Internals

TLS for PG-wire TCP connections uses `rustls` (pure Rust, no OpenSSL) and
lives in `tls.rs`. `Config::with_tls` takes a `TlsConfig`; `hyperdb-api`'s
connection builders and pools pass theirs through it. The rustdoc on
`client::tls` is the reference for the details:

- `TlsMode` follows libpq's `sslmode` names, with sqlx's choices where they
  differ: `Disable` is the default, and `Prefer` falls back to plaintext only
  when the server declines the `SSLRequest`, never after a failed handshake
  or certificate check.
- A configured root certificate is the only trust anchor. `VerifyFull`
  without one trusts the bundled `webpki-roots`; the OS store and
  `~/.postgresql/` are never read.
- A cancel request for a TLS session goes over TLS, never plaintext.
- Session resumption is disabled: `hyperd` aborts a resumed handshake with
  `internal_error`.

gRPC TLS is handled separately by `tonic`'s built-in TLS support, configured
via `GrpcConfig` (auto-detected from `https://` endpoints).

---

## Testing

### Integration Test Setup

Integration tests require a running Hyper server. The `tests/common/mod.rs` module
provides `TestServer`, which:

1. Starts a `HyperProcess` (via `hyperdb-api` dev-dependency)
2. Creates a temporary database
3. Provides `Config` and `Client` helpers for the test
4. Cleans up on drop

```rust
use crate::common::TestServer;

#[test]
fn test_something() -> hyperdb_api::Result<()> {
    let server = TestServer::new()?;
    let client = server.connect()?;
    // ... test with real server ...
    Ok(())
}
```

Test output (databases, logs) goes to `hyperdb-api-core/test_results/`.

### Running Tests

```bash
# All hyperdb-api-core tests (requires HYPERD_PATH or a discoverable .hyperd/current)
cargo test -p hyperdb-api-core

# Unit tests only (no server needed)
cargo test -p hyperdb-api-core --lib

# Specific test file
cargo test -p hyperdb-api-core --test client_tests
```

### TLS Tests

The unit tests in `tls.rs` run the client against a fake PG-wire server on a
local thread, which answers the `SSLRequest` and runs a rustls server. They
cover the verifiers, each `SSLRequest` answer, and cancel over TLS, which
`hyperd` cannot prove because it accepts a plaintext cancel too. The end-to-end tests against a real `hyperd`
started with `ssl_key` / `ssl_certificate` are in
`hyperdb-api/tests/tls_tests.rs`. Both generate their certificates at runtime
with `rcgen`.

### Writing New Tests

- Use `TestServer::new()` for tests that need a database
- Use `TestServer::without_database()` for tests that manage databases explicitly
- Use `#[test]` for sync tests, `#[tokio::test]` for async tests
- Keep test databases in `test_results/` (auto-managed by `TestServer`)
- gRPC tests live in `hyperdb-api/tests/` since they exercise the full stack

---

## Feature Flags

| Feature | Dependencies Added | What It Enables |
|---------|--------------------|-----------------|
| `salesforce-auth` | `hyperdb-api-salesforce`, `arrow` | `AuthenticatedGrpcClient`, `with_data_cloud_token()` on `GrpcConfig` |

Everything else (TCP clients, gRPC clients, TLS, auth) is always available.

---

## Design Decisions

### Why Two Client Types (Client / AsyncClient)?

The sync `Client` uses `std::net::TcpStream` + `std::sync::Mutex` for zero
async runtime overhead. The async `AsyncClient` uses `tokio::net::TcpStream` +
`tokio::sync::Mutex`. They share the same wire protocol logic via
`RawConnection<S>` / `AsyncRawConnection<S>`, parameterized over the stream type.

### Why GrpcClientSync Wraps GrpcClient?

`GrpcClientSync` creates an internal `tokio::runtime::Runtime` and blocks on
the async `GrpcClient`. This avoids duplicating the gRPC logic but adds ~1ms
runtime creation overhead. For sync-heavy workloads, create one `GrpcClientSync`
and reuse it.

### Why Format Code 2 (HyperBinary)?

Standard PostgreSQL binary format (code 1) uses big-endian. Hyper extends the
protocol with format code 2 for little-endian binary (`HyperBinary`), which
avoids byte-swapping on x86/ARM and enables zero-copy field access in
`StreamRow`. This is a Hyper-specific extension not supported by standard
PostgreSQL servers.

### Cancellable Trait Design

The `Cancellable` trait is intentionally minimal (`fn cancel(&self)` with no
return value and no arguments) to serve as an internal cleanup abstraction
usable from `Drop` impls. User-facing cancel APIs (`Client::cancel() -> Result`,
`GrpcClient::cancel_query(id) -> Result`) are separate and fallible.
See `cancel.rs` for the full rationale.

---

## Known Tech Debt

- `GrpcClient` duplicates query-building logic between `execute_query_with_options`
  and `execute_query_with_params_and_options` -- should be unified
- `AsyncClient` lacks a streaming query mode equivalent to `QueryStream`
- Connection pooling is not part of this crate. It lives in `hyperdb-api`'s
  `pool` module, which wraps `deadpool` behind its own `Pool` and
  `PooledConnection` types

---

## Related Documentation

- [Root DEVELOPMENT.md](../../DEVELOPMENT.md) -- workspace-wide build, test, CI
- [Protocol development guide](DEVELOPMENT-protocol.md) -- wire protocol details
- [Types development guide](DEVELOPMENT-types.md) -- type system and binary formats
- [hyperdb-api README](../../hyperdb-api/README.md) -- high-level API built on this crate
