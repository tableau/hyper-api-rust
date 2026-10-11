# AGENTS.md

This file provides guidance to AI coding assistants working with code in this repository.

**Subdirectory guidance:** The [`hyperdb-api-node/`](hyperdb-api-node/AGENTS.md) directory has its own `AGENTS.md` covering the Node.js/TypeScript bindings, napi-rs build system, and JS-specific patterns.

**Bootstrapping `hyperd`:** Contributors obtain the `hyperd` executable by
running `make download-hyperd` (or `.\build.ps1 download-hyperd`). The
implementation lives in the [`hyperdb-bootstrap`](hyperdb-bootstrap/) crate;
the pinned release is baked into
[`hyperdb-bootstrap/hyperd-version.toml`](hyperdb-bootstrap/hyperd-version.toml).
Bumping `hyperd` = edit that file (`version` + the four `[wheel_tag]` entries +
the four per-platform sha256s — there is no `build_id`), then let the
`fix(bootstrap):` commit drive the version via release-please (the crate uses
`version.workspace = true` — don't hand-edit a crate version). `hyperd` comes
out of the PyPI `tableauhyperapi` wheels, so **you don't compute the digests**:
read them straight off the JSON API with
`curl -s https://pypi.org/pypi/tableauhyperapi/<version>/json | jq -r '.urls[] | "\(.filename)  \(.digests.sha256)"'`.
The full repeatable procedure — verify the pin, run the suite, A/B benchmark
against the previous pin, and log the result — is captured in the
[`update-hyperd-release`](.claude/skills/update-hyperd-release/SKILL.md) skill.

Performance history is tracked in
[`docs/hyperd-release-benchmarks.md`](docs/hyperd-release-benchmarks.md), which
takes a row on **either** a `hyperd` pin bump **or** a material API change
(edition/MSRV migration, hot-path rewrite, codegen-affecting dependency bump).
Each row is the baseline the next A/B measures against, so conflating the two
variables makes an engine delta unattributable.

## Project Overview

This is a **pure-Rust implementation** of the Hyper database API, using the PostgreSQL wire protocol with Hyper-specific extensions. It allows Rust applications to create, read, and manipulate Hyper database files (.hyper) without any C library dependencies.

**Key characteristics:**

- 100% pure Rust (no FFI, no C dependencies)
- High performance on a single connection (100M-row benchmark, Apple M3 Max): 68.9M rows/sec inserts via the async `AsyncArrowInserter`, 25.0M rows/sec via the sync `Inserter`, 31.1M rows/sec full-scan queries via the sync path. See [docs/BENCHMARK_GUIDE.md](docs/BENCHMARK_GUIDE.md) for the multi-connection and per-platform figures.
- Independent library (can be extracted from this repository)
- Zero build system dependencies (uses standard Cargo)
- **No feature flags on `hyperdb-api`** — every capability of the flagship crate (TLS, pooling, geography, transactions, chrono) is always available. A few companion crates do carry optional features; see [Feature Flags](#feature-flags) for the complete list.

## Architecture

The codebase uses a **layered architecture**. The flagship user-facing crate is `hyperdb-api`; its implementation details live in `hyperdb-api-core`, which preserves three internal submodules (`types`, `protocol`, `client`) that contributors navigate independently. Two optional companion crates extend the public surface.

```text
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

Tests and examples spawn a real `hyperd`. Run `make download-hyperd` (or
`.\build.ps1 download-hyperd`) once: it installs the release pinned in
[`hyperdb-bootstrap/hyperd-version.toml`](hyperdb-bootstrap/hyperd-version.toml)
under `.hyperd/current/`. The `make`/`build.ps1` targets export `HYPERD_PATH`
themselves, so plain `make test` needs no further setup.

**Always use the pinned binary at `.hyperd/current`, never an ad-hoc `hyperd`
elsewhere on the machine.** That is the release this repo pins and the one CI
runs against — [`.github/workflows/ci.yml`](.github/workflows/ci.yml) sets
`HYPERD_PATH` to `.hyperd/current` — so local results stay comparable to CI. A
copy sitting somewhere else may be an unrelated or unversioned build, which
makes a local pass weaker than it looks.

For a bare `cargo` invocation the most robust option is to set `HYPERD_PATH` to
an **absolute** path — the executable and its containing directory are both
accepted, and CI uses the directory form. An absolute value is inherited
unchanged by any child process a test spawns, so it works for the *whole*
suite:

```bash
export HYPERD_PATH="$PWD/.hyperd/current"   # from the workspace root
cargo test -p hyperdb-mcp --test attach_tests
```

Leaving `HYPERD_PATH` **unset** also works for most local runs and is fine for a
quick single-crate check: `HyperProcess::new()` walks up from the working
directory to find `.hyperd/current/hyperd`, locating the pinned binary from
anywhere in the repo. The catch is that this discovery is relative to the
process's working directory, so it fails for any test that re-execs a child with
a relocated working directory and `HOME` — notably `hyperdb-mcp`'s
`recovery_tests` watchdog/health cases
(`slow_health_report_does_not_hold_engine_mutex` and the two
`slow_health_watchdog_reaps_hyperd_*`): the child lands outside the repo, its
upward walk finds no `.hyperd`, and it fails with `Hyper PID was not reported`.
Measured on `upstream/main`, unset gives `5 passed; 3 failed` there while an
absolute `HYPERD_PATH` gives `8 passed` — so prefer the absolute form as your
default and reserve unset for quick single-crate runs.

A **relative** path does not work. Cargo runs an integration test with the
working directory set to the *package* root (`hyperdb-mcp/`), not the
workspace root, so `HYPERD_PATH=.hyperd/current` resolves against the wrong
directory and every hyperd-backed test fails with `HYPERD_PATH set to
'.hyperd/current' but hyperd executable not found`.

The Makefile falls back to `.hyperd/current/hyperd` only when `HYPERD_PATH` is
**unset** — an exported value always wins. Don't leave a stale one in your
shell profile, or `make test` silently inherits it too.

Confirm the engine before trusting a run:

```bash
.hyperd/current/hyperd --version
```

It must report the `version` from `hyperdb-bootstrap/hyperd-version.toml`. If
it prints `__UNVERSIONED_HYPER__` or some other version, the local engine is
not the pinned one and the results are not comparable to CI.

### Editor Setup (VS Code / Windsurf / Cursor)

**The entire workspace is `edition = "2024"`** as of 1.0.0 — not just a few transitive deps. Older copies of the rust-analyzer binary bundled with the VS Code extension reject that outright:

```text
failed to interpret `cargo metadata`'s json: unknown variant `2024`
```

`rust-toolchain.toml` lists `rust-analyzer` in its `components`, so rustup installs a matching binary for you when the toolchain is provisioned — no manual `rustup component add` step. What you may still need is to point the extension at that binary rather than its bundled one, from your **user** settings (not workspace settings — we intentionally don't commit this, so contributors on newer extensions aren't forced to change anything):

```json
"rust-analyzer.server.path": "rust-analyzer"
```

The rustup-shipped binary tracks the active toolchain, so it stays in lockstep with `cargo`. `"rust-analyzer"` with no path resolves via `$PATH` — the rustup shim under `~/.cargo/bin` on Unix or `%USERPROFILE%\.cargo\bin` on Windows.

One rust-analyzer quirk worth knowing, unrelated to the edition: with the `compile-time` feature enabled, `query_as!` validation depends on `derive(Table)` having registered the type in the same proc-macro process. rust-analyzer expands macros lazily and from cache, so it can expand a `query_as!` in a process where no derive ran. Validation detects that and skips rather than reporting a false "not registered" error, so the editor should stay quiet on code that `cargo check` accepts.

### Common Commands

The repo provides a `Makefile` for Linux/macOS and a PowerShell equivalent
`build.ps1` for Windows. Both wrappers auto-set `HYPERD_PATH` for test/run
targets. Plain `cargo …` invocations also work — set an absolute `HYPERD_PATH`
(the robust default, e.g. `"$PWD/.hyperd/current"`), or leave it unset and let
the upward `.hyperd/current` walk resolve it for most single-crate runs.

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
`.hyperd/` is populated. Prefer an absolute `HYPERD_PATH` (e.g.
`HYPERD_PATH="$PWD/.hyperd/current" cargo test --workspace`): a whole-workspace
run includes the `recovery_tests` child-re-exec cases above, which need it. The
upward `.hyperd/current` discovery (leaving `HYPERD_PATH` unset) covers most
narrower single-crate runs.

### Running Individual Tests

```bash
# Run all tests in a specific crate
cargo test -p hyperdb-api

# Run a specific test
cargo test -p hyperdb-api test_name

# Run tests in a specific file (use the file stem)
cargo test -p hyperdb-api --test connection_tests
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

Three *other* crates do define features. `hyperdb-api` is flag-free; the workspace as a whole is not:

- **`hyperdb-api-core`** — `salesforce-auth` (off by default): pulls in `hyperdb-api-salesforce` and `arrow`. A plumbing detail; normally picked up transitively through `hyperdb-api`.
- **`hyperdb-api-derive`** — `compile-time` (off by default): enables compile-time SQL validation via `query_as!` and `derive(Table)` `#[hyperdb(register)]`, pulling in `hyperdb-compile-check`.
- **`hyperdb-bootstrap`** — `cli` (**on** by default): the `clap`/`anyhow`/`tracing-subscriber` command-line surface. Depend on it with `default-features = false` to use it as a library.

## Testing Structure

Tests are organized by crate:

```text
hyperdb-api/tests/              # Integration tests (high-level API)
hyperdb-api/tests/common/       # Shared test utilities
hyperdb-api-core/tests/         # Client-level integration tests
hyperdb-api-core/src/protocol/  # Unit tests (inline with code)
hyperdb-api-core/src/types/     # Unit tests (inline with code)
```

**Test utilities:**

- `hyperdb-api/tests/common/mod.rs` - Shared test helpers
- `hyperdb-api-core/tests/common/mod.rs` - Client test utilities
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

// Process in chunks (TCP: up to 65,536 rows per chunk)
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

Whenever an MCP tool is added, renamed, removed, or its parameters/behavior
change — and whenever a feature surfaces through the MCP (new file format, new
export target, new SQL capability worth highlighting) — update
[`hyperdb-mcp/src/readme.rs`](hyperdb-mcp/src/readme.rs) so the LLM-facing
README returned by the `get_readme` tool stays accurate. The structural test in
`hyperdb-mcp/tests/readme_tests.rs` enforces tool-name coverage; semantic
content (parameter rules, examples, SQL quirks) is human-maintained and won't
fail loudly when stale.

## Performance Considerations

- **Inserter API uses binary COPY protocol** - 10-100x faster than INSERT statements
- **Streaming results** - Always process in chunks, never load all rows
- **Arrow batching** - Use the async `AsyncArrowInserter` for maximum insert throughput: 68.9M rows/sec on a single connection, versus 25.0M rows/sec for the sync `Inserter`. Only the async variant is benchmarked at that rate — the sync `ArrowInserter` is not measured by the suite. Spending extra connections on an Arrow insert buys nothing on the benchmarked host.
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
- **`docs/superpowers/`** — planning artifacts kept in-repo and committed: design specs
  under `docs/superpowers/specs/YYYY-MM-DD-<topic>-design.md` and their implementation
  plans under `docs/superpowers/plans/YYYY-MM-DD-<feature>.md`. Non-trivial features are
  brainstormed into a spec, then a plan, before code; keeping both in the repo makes the
  design rationale reviewable alongside the change that implements it.

All public items have `///` doc comments. Module-level docs (`//!`) explain architecture and patterns. Examples are in `hyperdb-api/examples/` and tested via `run_all_examples.sh`. Companion crate examples in `hyperdb-api-salesforce/examples/` and `sea-query-hyperdb/examples/`. API docs are generated via `make doc`.

See [docs/RUST_DOCUMENTATION_STYLE.md](docs/RUST_DOCUMENTATION_STYLE.md) for the full documentation style guide.

## Commit and Contribution Conventions

This project uses [Conventional Commits](https://www.conventionalcommits.org/)
for commit messages. Versioning and changelog generation are **fully automated
by release-please**:
[`.github/workflows/release-please.yml`](.github/workflows/release-please.yml)
runs on every push to `main` and opens (or updates) a
`chore(main): release X.Y.Z` PR, driven by
[`release-please-config.json`](release-please-config.json) and
[`.release-please-manifest.json`](.release-please-manifest.json). It also
re-runs on the `release: published` event a hand-cut tag emits, so cutting the
tag re-anchors the next `-rc.N` correctly (#308). Never hand-edit a crate
version or the root `CHANGELOG.md`. See
[CONTRIBUTING.md](CONTRIBUTING.md#release-process) for the full flow and
[docs/GITHUB_OPERATIONS.md](docs/GITHUB_OPERATIONS.md#cutting-a-release) for the
maintainer steps.

While the workspace is on an `-rc.N` line, the rc counter increments
automatically from any conventional commit — no `Release-As:` footer needed —
because [`release-please-config.json`](release-please-config.json) sets
`prerelease`, `prerelease-type`, and `versioning`. Those keys must be removed
when the final release ships, or the next `fix:` computes another rc instead of
a stable patch. The mechanism and that removal step are documented once, in
[docs/GITHUB_OPERATIONS.md → Pre-releases](docs/GITHUB_OPERATIONS.md#pre-releases).

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
4. **Use `make test` instead of `cargo test`** to ensure `HYPERD_PATH` points at the pinned `.hyperd/current` engine CI uses, not an ad-hoc copy
5. **Profile in release mode** - Debug builds are not representative of performance
6. **Write 100% idiomatic Rust** - All code must follow Rust idioms and conventions. Flag any existing code that isn't idiomatic when you encounter it.
7. **Ban narrowing `as` casts on integers** - `as` casts between integer types of different widths (e.g. `i16 as u8`, `u32 as u8`, `i128 as i64`) are *truncating*, not saturating. They silently wrap or drop high bits in release builds and are a documented source of data-corruption bugs in this codebase (see `hyperdb_api::Row::get_numeric`, `hyperdb_api_core::types::Numeric::encode_int64`, and the Arrow decimal paths). Use `TryFrom` instead:
   - **Caller can tolerate failure** (returns `Option`/`Result`): `u8::try_from(x).ok()?` → propagates `None`.
   - **Caller knows it fits by validated invariant**: `u8::try_from(x).expect("<reason the invariant holds>")` → panics loudly with the invariant in the message.
   - **Always fits by type algebra** (e.g. `i128::to_le_bytes()`, `u32` → `i64`, same-width signed/unsigned where sign isn't meaningful): keep the direct conversion, no `TryFrom` needed.

   Rationale: `as` on integers silently corrupts wire data, scale values, and indices when invariants are ever violated; `TryFrom` makes every narrowing a named, visible branch in the code. Don't write `as u8`, `as i32`, etc. on an integer unless you can prove the conversion is always lossless by the source type's range (e.g. `bool as u8`, `u8 as u16`, `i8 as i16`, `i32 as i64`). If in doubt, use `TryFrom`.

   When reviewing existing code or fixing bugs, flag and convert any narrowing `as` casts you encounter, even if they aren't the proximate cause of the bug — they're a latent-corruption vector and cheap to fix in the same change.

8. **Update the *per-crate* `CHANGELOG.md` for user-visible crate-level
 changes.** When a PR adds, changes, or removes any public API surface in a
 publishable crate (`hyperdb-api`, `hyperdb-api-core`, `hyperdb-compile-check`,
 `hyperdb-api-derive`, `hyperdb-api-node`, `hyperdb-api-salesforce`,
 `hyperdb-bootstrap`, `hyperdb-mcp`, `sea-query-hyperdb`), append a bullet to the
 `## [Unreleased]` section of that crate's `CHANGELOG.md` under the
 appropriate [Keep a Changelog](https://keepachangelog.com/) heading
 (`### Added`, `### Changed`, `### Deprecated`, `### Removed`, `### Fixed`,
 `### Security`). Internal refactors that don't change the public API surface
 do not require a changelog entry.

 **Which changelog files you may edit** — this is the part that trips people up, because [CONTRIBUTING.md](CONTRIBUTING.md#what-contributors-do) says contributors do *not* hand-edit changelogs. Both rules are correct; they govern different files:

- **Root [`CHANGELOG.md`](CHANGELOG.md) — never hand-edit.** It is release-please-generated, has no `## [Unreleased]` section, and is the only `changelog-path` in `release-please-config.json`.
- **The nine per-crate `CHANGELOG.md` files — hand-maintained.** Each carries exactly one `## [Unreleased]` section and none appear in release-please's `packages` or `extra-files`. This reminder applies to these. `hyperdb-compile-check` is the ninth: it is published (`release.yml` does so explicitly, since it declares its own `[workspace]` and `--workspace` cannot see it) but was missing from this list, which is why it had no changelog until 1.0.0.
- **The npm sub-package changelogs** under `hyperdb-api-node/npm/*/` and `hyperdb-mcp/npm/*/` — leave alone; they have no `## [Unreleased]` section.

 Nothing rolls a per-crate `## [Unreleased]` section over into a dated one
 when a release ships; that is a manual maintainer step
 ([docs/GITHUB_OPERATIONS.md → Rolling over the per-crate
 changelogs](docs/GITHUB_OPERATIONS.md#rolling-over-the-per-crate-changelogs)).
 So an entry sitting under "Unreleased" does **not** mean the work is
 unshipped — check the root `CHANGELOG.md` for that. Append your bullet to the
 existing section rather than adding a second `### Fixed` sibling (MD024).

1. **Never invent `hyperd` flags or engine parameters.** Obtain `hyperd` via
 `make download-hyperd` (it bootstraps the release pinned in
 `hyperdb-bootstrap/hyperd-version.toml`) and start servers through the
 documented path — `HyperProcess::new()` in tests, the Makefile targets, or
 `HYPERD_PATH` as described above. If you think a startup flag or parameter is
 needed, confirm it against `hyperd --help`, an existing script, or this file
 **before** relying on it. Fabricated `hyperd` parameters silently fail
 against the real binary — they have previously made tests hang while
 appearing to "run."

2. **Never report a test/build as passing without seeing real output.** Check exit codes. If a command produces no output for ~30s, treat it as **hanging/failed**, not passing, and say so explicitly. A green claim backed by no captured output is a defect, not a result — tests here start a real `hyperd` subprocess (`HyperProcess::drop()` stops it), so a misconfigured server hangs rather than erroring cleanly.

3. **Run markdownlint on any Markdown you touch, before committing.** It is
 **not** a CI gate, so nothing catches these for you — the only feedback is the
 editor extension, and an agent working headless gets none at all.

 ```bash
 npx markdownlint-cli2
 ```

 No arguments: `.markdownlint-cli2.jsonc` supplies the globs and the path
 exclusions. Rules live in `.markdownlint.json` (shared with the editor
 extension); `.markdownlintignore` exists only because the extension reads it
 and `markdownlint-cli2` does not.

 **There is a pre-existing backlog**, so a nonzero count is not automatically
 yours. Judge new findings against the file's prior state — `git show
 upstream/main:<path>` and re-lint — rather than assuming, or you will "fix"
 things that were never broken and miss the ones you introduced.

 Four traps that have actually bitten:

- **Duplicate `### Fixed` / `### Added` siblings under one `## [Unreleased]`**
 (MD024). Changelogs here often already have the section further down. Merge
 your bullet into the existing one instead of adding a second heading — that
 also keeps [Keep a Changelog](https://keepachangelog.com/) ordering.
- **Bare ``` fences** (MD040) need a language. Use `text` for command output,
 ASCII diagrams, error messages, and templates.
- **Never bulk-auto-fix fences with a naive script.** A language-tagged
 opening fence does not match a bare-fence test, so the *closing* fence gets
 mistaken for an opening one and tagged — silently turning a terminator into a
 new block. This corrupted 176 fences across 22 files once. Any such pass must
 track fence state; prefer `markdownlint-cli2 --fix`, which is safe, and note
 that it cannot fix MD040 because choosing a language needs judgement.
- **A nested `.markdownlint.json` replaces the root config rather than merging
 with it**, so a new one must `extends` the root or every rule there reverts to
 default. Dropping that line from `docs/superpowers/.markdownlint.json` turns 5
 findings into 92 — 70 of them the MD060 disabled just below. It hides well: it
 makes the *linter* wrong, not the document.

 Beware format-on-save: a Markdown formatter reformatting tables to satisfy
 MD060 once stripped the README's badge links (`[![CI](img)](target)` became
 `![CI](img)`) and split an inline link across a newline into two links. MD060
 is disabled in `.markdownlint.json` for exactly this reason.
