# AGENTS.md

This file provides guidance to AI coding assistants working with code in this repository.

**Subdirectory guidance:** The [`hyperdb-api-node/`](hyperdb-api-node/AGENTS.md) directory has its own `AGENTS.md` covering the Node.js/TypeScript bindings, napi-rs build system, and JS-specific patterns.

**Bootstrapping `hyperd`:** Contributors obtain the `hyperd` executable by running `make download-hyperd` (or `.\build.ps1 download-hyperd`). The implementation lives in the [`hyperdb-bootstrap`](hyperdb-bootstrap/) crate; the pinned release is baked into [`hyperdb-bootstrap/hyperd-version.toml`](hyperdb-bootstrap/hyperd-version.toml). Bumping `hyperd` = edit that file (version + build_id + per-platform sha256s), bump the crate version, publish.

## Project Overview

This is a **pure-Rust implementation** of the Hyper database API, using the PostgreSQL wire protocol with Hyper-specific extensions. It allows Rust applications to create, read, and manipulate Hyper database files (.hyper) without any C library dependencies.

**Key characteristics:**
- 100% pure Rust (no FFI, no C dependencies)
- High performance (22-24M rows/sec inserts, 18M rows/sec queries)
- Independent library (can be extracted from this repository)
- Zero build system dependencies (uses standard Cargo)
- **Zero feature flags** — all capabilities are always available

## Architecture

The codebase uses a **layered architecture**. The flagship user-facing crate is `hyperdb-api`; its implementation details live in `hyperdb-api-core`, which preserves three internal submodules (`types`, `protocol`, `client`) that contributors navigate independently. Two optional companion crates extend the public surface.

```
┌─────────────────────────────────────────────────────┐
│  hyperdb-api (High-level API, public)               │
│  - Connection, AsyncConnection, HyperProcess        │
│  - Inserter, Catalog, Arrow integration             │
│  - Pool, gRPC, Transactions                         │
└────────────────┬────────────────────────────────────┘
                 │ depends on (exact-match pin)
                 ▼
┌─────────────────────────────────────────────────────┐
│  hyperdb-api-core (internal implementation detail)  │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │  src/client/ (Connection Management)          │  │
│  │  - Sync/Async TCP clients                     │  │
│  │  - Authentication (MD5, SCRAM-SHA-256)        │  │
│  │  - gRPC transport & TLS (rustls)              │  │
│  └────────────────┬──────────────────────────────┘  │
│                   │                                 │
│  ┌────────────────▼──────────────────────────────┐  │
│  │  src/protocol/ (Wire Protocol)                │  │
│  │  - PostgreSQL protocol messages               │  │
│  │  - HyperBinary COPY format                    │  │
│  │  - Message parsing/encoding                   │  │
│  └────────────────┬──────────────────────────────┘  │
│                   │                                 │
│  ┌────────────────▼──────────────────────────────┐  │
│  │  src/types/ (Type System)                     │  │
│  │  - LittleEndian binary encoding               │  │
│  │  - Type conversions (Date, Numeric, Geography)│  │
│  │  - SQL type definitions (Oid, SqlType)        │  │
│  └───────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────┘

Companion crates (optional, add when needed):
┌──────────────────────────┐  ┌──────────────────────────┐
│  hyperdb-api-salesforce  │  │  sea-query-hyperdb       │
│  Salesforce Data Cloud   │  │  HyperDB dialect backend │
│  OAuth authentication    │  │  for sea-query           │
└──────────────────────────┘  └──────────────────────────┘
```

**Important:** `hyperdb-api-core` is published to crates.io (Cargo requires it, because `hyperdb-api` depends on it) but it is **not a public API** — users should depend on `hyperdb-api` only. See [`hyperdb-api-core/README.md`](hyperdb-api-core/README.md) for the "forever internal" positioning.

Each submodule has clear boundaries. Always work within the appropriate layer:
- Type encoding issues → `hyperdb-api-core/src/types/`
- Protocol message issues → `hyperdb-api-core/src/protocol/`
- Connection/transport issues → `hyperdb-api-core/src/client/`
- High-level API issues → `hyperdb-api`
- Salesforce OAuth → `hyperdb-api-salesforce`
- sea-query SQL generation → `sea-query-hyperdb`
- MCP server (CLI product) → `hyperdb-mcp`
- `hyperd` bootstrap helper → `hyperdb-bootstrap`

## Dual Architecture: Sync and Async

The codebase provides **both** synchronous and asynchronous APIs:

- **Sync API:** `Connection`, `Inserter`, `HyperProcess`
  - Used in `hyperdb-api/src/connection.rs`, `inserter.rs`
  - Blocking I/O, simpler API surface

- **Async API:** `AsyncConnection`, `AsyncArrowInserter`, connection pooling
  - Used in `hyperdb-api/src/async_connection.rs`, `async_arrow_inserter.rs`
  - Tokio-based, non-blocking I/O
  - Connection pooling via `pool` module

When implementing features, consider whether both sync and async variants need updates.

## Transport Layers

The API supports **two transport protocols**:

1. **TCP Transport (default):** Direct PostgreSQL wire protocol
   - Uses `hyperdb_api_core::client::{Connection, AsyncConnection}`
   - Primary transport for most operations
   - Supports authentication, TLS, CREATE/INSERT/UPDATE/DELETE

2. **gRPC Transport:** Read-only queries with Arrow IPC format
   - Uses `hyperdb_api_core::client::grpc::GrpcClient`
   - Exposed via `hyperdb_api::grpc::GrpcConnection`
   - Read-only (queries only, no mutations)
   - Always available
   - Used for high-performance streaming queries

**Architecture pattern:** The high-level `Connection` abstracts over transports via the `transport` module (`hyperdb-api/src/transport.rs`).

## Build and Test Commands

### Environment Setup

**CRITICAL:** The `HYPERD_PATH` environment variable must point to the `hyperd` executable. The Makefile auto-detects it in known locations, but you may need to set it manually:

```bash
export HYPERD_PATH=/path/to/hyperd
```

The Makefile will auto-detect `hyperd` in common locations if `HYPERD_PATH` is not set.

**Default for this workstation:** always use `HYPERD_PATH=~/dev/bin/hyperd` when running `cargo test`/`cargo run`/etc. directly (the Makefile targets already export it, so plain `make test` is also fine). Example:

```bash
HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test attach_tests
```

### Editor Setup (VS Code / Windsurf / Cursor)

A few of our transitive deps (`rmcp`, `rmcp-macros`, `base64ct`, `clap_lex`) use `edition = "2024"`. Older copies of the rust-analyzer binary bundled with the VS Code extension reject that with:

```
failed to interpret `cargo metadata`'s json: unknown variant `2024`
```

If you hit this, install rust-analyzer via rustup and point the extension at it from your **user** settings (not workspace settings — we intentionally don't commit this so contributors on newer extensions aren't forced to change anything):

```bash
rustup component add rust-analyzer
```

Then in your user `settings.json`:

```json
"rust-analyzer.server.path": "rust-analyzer"
```

The rustup-shipped binary tracks the active toolchain (1.85+ supports edition 2024) so it stays in lockstep with `cargo`. `"rust-analyzer"` with no path resolves via `$PATH` — the rustup shim under `~/.cargo/bin` on Unix or `%USERPROFILE%\.cargo\bin` on Windows.

### Common Commands

The repo provides a `Makefile` for Linux/macOS and a PowerShell equivalent
`build.ps1` for Windows. Both wrappers auto-set `HYPERD_PATH` for test/run
targets. Plain `cargo …` invocations also work but require setting
`HYPERD_PATH` manually.

**Linux / macOS** (`bash`):

```bash
make build               # Build all crates (debug)
make build-release       # Build release binaries
make test                # Run all tests (debug) - auto-sets HYPERD_PATH
make test-release        # Run tests (release mode)
make examples            # Run all examples (or: ./run_all_examples.sh)
make doc                 # Generate documentation (Hyper crates only, no dependencies)
make clean               # Clean build artifacts AND test files (.hyper, logs)
make clean-test-files    # Clean only test-generated files
```

**Windows** (`pwsh` / PowerShell):

```powershell
.\build.ps1 build              # Build all crates (debug)
.\build.ps1 build-release      # Build release binaries
.\build.ps1 test               # Run all tests (debug) - auto-sets HYPERD_PATH
.\build.ps1 test-release       # Run tests (release mode)
.\build.ps1 examples           # Run all examples (or: .\run_all_examples.ps1)
.\build.ps1 doc                # Generate documentation (Hyper crates only, no dependencies)
.\build.ps1 clean              # Clean build artifacts AND test files (.hyper, logs)
.\build.ps1 clean-test-files   # Clean only test-generated files
```

The bare `cargo` equivalent for any target works on either platform once
`HYPERD_PATH` is set (e.g. `HYPERD_PATH=~/dev/bin/hyperd cargo test --workspace`).

### Running Individual Tests

```bash
# Run all tests in a specific crate
cargo test -p hyperdb-api

# Run a specific test
cargo test -p hyperdb-api test_name

# Run tests in a specific file (use module path)
cargo test -p hyperdb-api --test integration_test
```

### Running Examples

```bash
# Run a specific example
cargo run -p hyperdb-api --example insert_data_into_single_table

# Run companion crate examples
cargo run -p sea-query-hyperdb --example basic_usage
cargo run -p hyperdb-api-salesforce --example salesforce_auth_example

# Run all examples
./run_all_examples.sh
```

## Feature Flags

The `hyperdb-api` crate has **no feature flags**. All capabilities (TLS, pooling,
geography, transactions, chrono) are always enabled. This simplifies dependency
management and matches the C++/Python/Java APIs.

Domain-specific functionality lives in companion crates:
- **`sea-query-hyperdb`** — HyperDB dialect backend for `sea-query`
- **`hyperdb-api-salesforce`** — Salesforce Data Cloud OAuth authentication

## Testing Structure

Tests are organized by crate:

```
hyperdb-api/tests/              # Integration tests (high-level API)
hyperdb-api/tests/common/       # Shared test utilities
hyperdb-api-core/tests/         # Client-level integration tests
hyperdb-api-core/src/protocol/  # Unit tests (inline with code)
hyperdb-api-core/src/types/     # Unit tests (inline with code)
```

**Test utilities:**
- `hyperdb-api/tests/common/mod.rs` - Shared test helpers
- `hyperdb-api-core/src/client/test_util.rs` - Client test utilities
- Both use `HyperProcess::new()` to start temporary `hyperd` servers

**Pattern:** Tests create temporary `.hyper` files and clean them up automatically. The `make clean-test-files` command removes any leftover test artifacts.

## Key Implementation Patterns

### 1. Error Handling

All public APIs return `Result<T, Error>` where `Error` is from `hyperdb_api::Error`:

```rust
use hyperdb_api::{Result, Error};

pub fn some_function() -> Result<()> {
    // Use ? operator for error propagation
    let conn = Connection::connect(...)?;
    conn.execute_command("...")?;
    Ok(())
}
```

Error types are defined in:
- `hyperdb-api/src/error.rs` - High-level errors with `ErrorKind` variants
- `hyperdb-api-core/src/client/error.rs` - Client-level errors
- `hyperdb-api-core/src/protocol/` - Protocol errors (minimal, mostly I/O)

### 2. Streaming Query Results

Query results are **always streaming** to maintain constant memory usage:

```rust
let mut result = conn.execute_query("SELECT * FROM large_table")?;

// Process in chunks (default: 16384 rows per chunk)
while let Some(chunk) = result.next_chunk()? {
    for row in &chunk {
        // Process row
    }
}
```

**Important:** Never load entire result sets into memory. Always use chunk-based iteration.

### 3. Type Conversions

Type conversions follow these patterns:

- **Reading values:** Use `row.get::<T>(col_index)` with type inference
  ```rust
  let id: Option<i32> = row.get(0);
  let name: Option<String> = row.get(1);
  ```

- **Writing values:** Implement `ToSqlParam` or `IntoValue` traits
  - `ToSqlParam` - For query parameters (text format)
  - `IntoValue` - For inserter values (binary format)

All conversions are in `hyperdb-api-core/src/types/types.rs` and `traits.rs`.

### 4. Connection Lifecycle

Connections have explicit lifecycle management:

```rust
// Start a server (manages hyperd process)
let hyper = HyperProcess::new(None, None)?;
let endpoint = hyper.require_endpoint()?;

// Connect (opens TCP connection + PostgreSQL handshake)
let conn = Connection::connect(endpoint, "db.hyper", CreateMode::CreateIfNotExists)?;

// Use connection...

// Close explicitly (or drop will close)
conn.close()?;

// HyperProcess drop handler stops the hyperd process
```

**Note:** `HyperProcess::drop()` automatically stops the `hyperd` subprocess. Tests rely on this for cleanup.

### 5. Arrow Integration

The codebase uses Apache Arrow for high-performance data exchange:

- **Reading:** `ArrowReader` reads query results as Arrow RecordBatches
- **Writing:** `ArrowInserter` / `AsyncArrowInserter` write Arrow data to Hyper
- **gRPC:** All gRPC queries return Arrow IPC format

Arrow types are in `hyperdb-api/src/arrow_result.rs`, `arrow_reader.rs`, `arrow_inserter.rs`.

## Common Development Scenarios

### Adding a New SQL Type

1. Add type definition to `hyperdb-api-core/src/types/oid.rs` (OID constant)
2. Add SQL type constructor to `hyperdb-api-core/src/types/sql_type.rs`
3. Implement `FromBinaryValue` trait in `hyperdb-api-core/src/types/types.rs`
4. Implement `ToSqlParam` for query parameters (text format)
5. Implement `IntoValue` for inserter (binary format)
6. Add tests in `hyperdb-api-core/src/types/types.rs`

### Adding a New Connection Feature

1. Implement protocol-level support in `hyperdb-api-core/src/protocol/`
2. Add client-level support in `hyperdb-api-core/src/client/client.rs` or `async_client.rs`
3. Expose high-level API in `hyperdb-api/src/connection.rs` or `async_connection.rs`
4. Add integration tests in `hyperdb-api/tests/`
5. Document in the appropriate `README.md` (user-facing usage) and `DEVELOPMENT.md` (internals)

### Adding a New Transport

1. Implement transport interface in `hyperdb-api-core/src/client/`
2. Add transport variant to `hyperdb-api/src/transport.rs`
3. Update `Connection::new()` to support new transport
4. Add tests in `hyperdb-api-core/tests/` and `hyperdb-api/tests/`

### Modifying the HyperDB MCP Tool Surface

Whenever an MCP tool is added, renamed, removed, or its parameters/behavior change — and whenever a feature surfaces through the MCP (new file format, new export target, new SQL capability worth highlighting) — update [`hyperdb-mcp/src/readme.rs`](hyperdb-mcp/src/readme.rs) so the LLM-facing README returned by the `get_readme` tool stays accurate. The structural test in `hyperdb-mcp/tests/readme_tests.rs` enforces tool-name coverage; semantic content (parameter rules, examples, SQL quirks) is human-maintained and won't fail loudly when stale.

## Performance Considerations

- **Inserter API uses binary COPY protocol** - 10-100x faster than INSERT statements
- **Streaming results** - Always process in chunks, never load all rows
- **Arrow batching** - Use `ArrowInserter` for maximum throughput (22M+ rows/sec)
- **Release builds** - Use `--release` for benchmarks (debug is 10x+ slower)
- **Connection pooling** - Use `pool` module for async high-concurrency scenarios

See `docs/BENCHMARK_GUIDE.md` for benchmark methodology and reproduction.

## Windows vs Unix/MacOS Differences

The codebase handles platform-specific IPC transport:

- **Unix/MacOS:** Uses Unix Domain Sockets (UDS) by default, falls back to TCP
- **Windows:** Uses Named Pipes, falls back to TCP

IPC detection is in `hyperdb-api/src/process.rs`. Most code is platform-agnostic.

## Documentation Conventions

Documentation is split by audience:
- **READMEs** (`README.md`) — user-facing: what the crate does, quick start, usage examples
- **DEVELOPMENT.md** — contributor-facing: internal architecture, design decisions, how to extend, testing
- **Source code** (`///` and `//!`) — implementation details co-located with code
- **`docs/`** — cross-cutting topics: performance, benchmarks, transactions, comparisons

All public items have `///` doc comments. Module-level docs (`//!`) explain architecture and patterns. Examples are in `hyperdb-api/examples/` and tested via `run_all_examples.sh`. Companion crate examples in `hyperdb-api-salesforce/examples/` and `sea-query-hyperdb/examples/`. API docs are generated via `make doc`.

See [docs/RUST_DOCUMENTATION_STYLE.md](docs/RUST_DOCUMENTATION_STYLE.md) for the full documentation style guide.

## Commit and Contribution Conventions

This project uses [Conventional Commits](https://www.conventionalcommits.org/) for commit messages. A [`release-please-config.json`](../release-please-config.json) is checked in for future use, but no GitHub Actions workflow currently invokes Release Please — versioning and changelog updates are maintained manually today (see [CONTRIBUTING.md](CONTRIBUTING.md#release-process)).

All commit messages **must** follow the format `<type>(<scope>): <subject>` — for the full specification including commit types, version impact, and examples, see [CONTRIBUTING.md](CONTRIBUTING.md#commit-message-format).

## Git Workflow Notes

- Main branch: `main`
- Test artifacts (`.hyper` files, logs) are gitignored
- Use `make clean-test-files` before committing to remove test debris
- CI/CD should set `HYPERD_PATH` appropriately

## Codebase-Specific Reminders

1. **Never commit `.hyper` files or `hyperd*.log` files** - These are test artifacts
2. **Always propagate errors with `?`** - Don't panic in library code
3. **Test both sync and async APIs** when adding features
4. **Use `make test` instead of `cargo test`** to ensure `HYPERD_PATH` is set
5. **Profile in release mode** - Debug builds are not representative of performance
6. **Write 100% idiomatic Rust** - All code must follow Rust idioms and conventions. Flag any existing code that isn't idiomatic when you encounter it.
7. **Ban narrowing `as` casts on integers** - `as` casts between integer types of different widths (e.g. `i16 as u8`, `u32 as u8`, `i128 as i64`) are *truncating*, not saturating. They silently wrap or drop high bits in release builds and are a documented source of data-corruption bugs in this codebase (see `hyperdb_api::Row::get_numeric`, `hyperdb_api_core::types::Numeric::encode_int64`, and the Arrow decimal paths). Use `TryFrom` instead:
   - **Caller can tolerate failure** (returns `Option`/`Result`): `u8::try_from(x).ok()?` → propagates `None`.
   - **Caller knows it fits by validated invariant**: `u8::try_from(x).expect("<reason the invariant holds>")` → panics loudly with the invariant in the message.
   - **Always fits by type algebra** (e.g. `i128::to_le_bytes()`, `u32` → `i64`, same-width signed/unsigned where sign isn't meaningful): keep the direct conversion, no `TryFrom` needed.

   Rationale: `as` on integers silently corrupts wire data, scale values, and indices when invariants are ever violated; `TryFrom` makes every narrowing a named, visible branch in the code. Don't write `as u8`, `as i32`, etc. on an integer unless you can prove the conversion is always lossless by the source type's range (e.g. `bool as u8`, `u8 as u16`, `i8 as i16`, `i32 as i64`). If in doubt, use `TryFrom`.

   When reviewing existing code or fixing bugs, flag and convert any narrowing `as` casts you encounter, even if they aren't the proximate cause of the bug — they're a latent-corruption vector and cheap to fix in the same change.

8. **Update `CHANGELOG.md` for user-visible crate-level changes.** When a PR adds, changes, or removes any public API surface in a publishable crate (`hyperdb-api`, `hyperdb-api-core`, `hyperdb-api-node`, `hyperdb-api-salesforce`, `hyperdb-bootstrap`, `hyperdb-mcp`, `sea-query-hyperdb`), append a bullet to the `## [Unreleased]` section of that crate's `CHANGELOG.md` under the appropriate [Keep a Changelog](https://keepachangelog.com/) heading (`### Added`, `### Changed`, `### Deprecated`, `### Removed`, `### Fixed`, `### Security`). Internal refactors that don't change the public API surface do not require a changelog entry. The `## [Unreleased]` section is promoted to a dated `## [X.Y.Z] - YYYY-MM-DD` section by the maintainer at release time. See [CONTRIBUTING.md](CONTRIBUTING.md#authoring-changes-every-contributor) for the full policy.

9. **Never invent `hyperd` flags or engine parameters.** Obtain `hyperd` via `make download-hyperd` (it bootstraps the release pinned in `hyperdb-bootstrap/hyperd-version.toml`) and start servers through the documented path — `HyperProcess::new()` in tests, the Makefile targets, or `HYPERD_PATH` as described above. If you think a startup flag or parameter is needed, confirm it against `hyperd --help`, an existing script, or this file **before** relying on it. Fabricated `hyperd` parameters silently fail against the real binary — they have previously made tests hang while appearing to "run."

10. **Never report a test/build as passing without seeing real output.** Check exit codes. If a command produces no output for ~30s, treat it as **hanging/failed**, not passing, and say so explicitly. A green claim backed by no captured output is a defect, not a result — tests here start a real `hyperd` subprocess (`HyperProcess::drop()` stops it), so a misconfigured server hangs rather than erroring cleanly.
