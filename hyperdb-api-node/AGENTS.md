# AGENTS.md — hyperdb-api-node

This file provides guidance to AI coding assistants working on the Node.js bindings.

For the overall repository architecture, Rust crate structure, and contribution conventions, see the [root AGENTS.md](../AGENTS.md).

For documentation conventions specific to JavaScript/TypeScript, see the [Documentation Style Guide](DOCUMENTATION_STYLE.md).

## Overview

`hyperdb-api-node` is a **napi-rs** native addon that exposes the Rust `hyperdb-api` crate to JavaScript and TypeScript. The Rust side (`src/*.rs`) compiles to a `.node` shared library; the JS side (`index.js`, `index.d.ts`, `pool.mjs`, `arrow.mjs`) adds ergonomic wrappers on top.

```
┌──────────────────────────────────────────────────┐
│  JS/TS application code                          │
├──────────────────────────────────────────────────┤
│  index.js     — native loader + JS extensions    │
│  index.d.ts   — hand-written TypeScript decls    │
│  pool.mjs     — ConnectionPool (pure JS)         │
│  arrow.mjs    — Arrow convenience helpers        │
├──────────────────────────────────────────────────┤
│  src/*.rs     — napi-rs bindings (Rust → JS)     │
│                 Uses hyperdb-api + hyperdb-api-core       │
├──────────────────────────────────────────────────┤
│  hyperdb-api — Pure Rust high-level API         │
│  hyperdb-api-core::client — TCP/gRPC, wire protocol          │
└──────────────────────────────────────────────────┘
```

## Key Files

| File | Purpose |
|------|---------|
| `src/lib.rs` | Rust module declarations |
| `src/connection.rs` | napi-rs `Connection`, `ConnectionBuilder` |
| `src/process.rs` | napi-rs `HyperProcess` |
| `src/inserter.rs` | napi-rs `Inserter` |
| `src/catalog.rs` | napi-rs `Catalog` |
| `src/result.rs` | napi-rs `RowData`, `ResultColumnInfo` |
| `src/query_stream.rs` | napi-rs `QueryStream` |
| `src/columnar.rs` | napi-rs `ColumnarStream`, `ColumnarChunk` |
| `src/types.rs` | napi-rs `SqlType`, `TableDefinition`, `CreateMode` |
| `src/query_stats.rs` | Query statistics |
| `index.js` | Native binding loader + JS extensions (tagged templates, parameterized queries, event hooks, `Symbol.asyncDispose`, `toJSON`) |
| `index.d.ts` | Hand-written TypeScript declarations (full IntelliSense) |
| `pool.mjs` | `ConnectionPool` — pure JS connection pooling |
| `arrow.mjs` | Arrow convenience helpers (`tableFromQuery`, `insertFromTable`, etc.) |
| `Cargo.toml` | Rust crate config (cdylib, depends on `hyperdb-api` + `hyperdb-api-core`) |
| `package.json` | npm package config, build scripts, platform triples |

## Build and Test Commands

**Prerequisites:** Rust toolchain, Node.js >= 21, `HYPERD_PATH` set.

```bash
cd hyperdb-api-node

# Install JS dependencies (includes @napi-rs/cli)
npm install

# Build native addon (debug — fast compile, slow runtime)
npm run build:debug

# Build native addon (release — slow compile, fast runtime)
npm run build

# Run smoke tests
npm test

# Run benchmarks (build release first!)
npm run build && npm run benchmark
```

### What `npm run build` does

1. `cargo build -p hyperdb-api-node --release` — compiles the Rust crate to a `.node` shared library
2. `node scripts/copy-node.js release` — copies the build artifact to the project root with the platform-specific name (e.g., `hyperdb-api-node.darwin-arm64.node`)

## Architecture Patterns

### Rust ↔ JS Bridge

- Each Rust struct in `src/*.rs` uses `#[napi]` attribute macros to expose constructors, methods, and properties to JS.
- **Sync Rust → Async JS:** Blocking Rust methods are wrapped with `tokio::task::spawn_blocking` and return `Promise` to JavaScript.
- **Thread safety:** Connections are wrapped in `Arc<Mutex<...>>` for safe concurrent access from the JS event loop.

### JS Extensions in `index.js`

The native binding loader (`index.js`) is **not just a loader** — it adds significant functionality on top of the napi-rs exports:

- **Parameterized queries** (`executeQueryParams`, `executeCommandParams`) — `$1`/`$2` placeholder substitution via `escapeParam()`
- **Tagged template literals** (`conn.sql\`...\``, `conn.command\`...\``) — safe SQL interpolation
- **Query event hooks** (`conn.on('query', ...)`) — wraps `executeQuery`/`executeCommand` with timing
- **`Symbol.asyncDispose` / `Symbol.dispose`** — resource management for `await using`
- **`RowData.toJSON()`** — serialization with optional column names
- **`QueryStream[Symbol.asyncIterator]`** — `for await (const row of stream)` support
- **`createExtractTable()`** — Tableau Extract schema helper

When modifying connection behavior, check both `src/connection.rs` (Rust) and `index.js` (JS wrappers).

### TypeScript Declarations

`index.d.ts` is **hand-written**, not auto-generated. It must be updated manually when:
- A new class or method is added in the Rust source
- A new JS extension is added in `index.js`
- Method signatures change

### Pure JS Modules

- **`pool.mjs`** — `ConnectionPool` with configurable min/max, idle timeout, acquire timeout. Entirely in JS, wraps `Connection`.
- **`arrow.mjs`** — Convenience wrappers around `executeQueryToArrow()` using the `apache-arrow` peer dependency. Also entirely in JS.

## Testing

- **Smoke tests:** `__test__/smoke.mjs` — covers all major features (connection, queries, inserts, streams, pool, tagged templates, BigInt, dates, JSON, event hooks)
- **Benchmarks:** `__test__/benchmark.mjs` — insert and query performance with configurable row counts
- Tests require `HYPERD_PATH` to be set
- Test artifacts go into `test_results/` (gitignored)

## npm Publishing

Uses napi-rs platform packages for cross-platform prebuilt binaries:

| Platform | Package |
|----------|---------|
| macOS ARM64 | `hyperdb-api-node-darwin-arm64` |
| Linux x64 (glibc) | `hyperdb-api-node-linux-x64-gnu` |
| Linux x64 (musl) | `hyperdb-api-node-linux-x64-musl` |
| Linux ARM64 | `hyperdb-api-node-linux-arm64-gnu` |
| Windows x64 | `hyperdb-api-node-win32-x64-msvc` |

macOS x64 (Intel) is currently disabled — `macos-13` GHA runners are unreliable.

CI builds all enabled platforms on push. Publishing is driven by release-please.

## Common Development Scenarios

### Adding a New Method to Connection

1. Implement in `src/connection.rs` with `#[napi]` attribute
2. If it needs a JS wrapper (params, events, etc.), add to `index.js`
3. Add TypeScript declaration to `index.d.ts`
4. Add test case to `__test__/smoke.mjs`

### Adding a New napi-rs Class

1. Create `src/<name>.rs`, add module to `src/lib.rs`
2. Use `#[napi]` on the struct and its methods
3. Export from `index.js` (it re-exports all nativeBinding properties)
4. Add TypeScript declarations to `index.d.ts`
5. Add tests to `__test__/smoke.mjs`

### Modifying Type Mappings

- **Rust → JS:** Reading conversions are in `src/result.rs` (`RowData` getters)
- **JS → Rust:** Writing conversions are in `src/inserter.rs` and `index.js` (`escapeParam`)
- Update the type mapping tables in `README.md` if you change behavior

## Documentation Conventions

This package splits its documentation across two files:

- **[README.md](README.md)** — user-facing content: features, installation, Quick Start, API Reference, Arrow integration, benchmarks, examples. This is what users see on npmjs.com.
- **[DEVELOPMENT.md](DEVELOPMENT.md)** — contributor internals: architecture, module map, building from source, testing, publishing, adding new methods/extensions, design decisions, future enhancements.

When making changes:
- **New user-facing API?** Update the API Reference section in `README.md`.
- **New build step, design decision, or contributor workflow?** Update `DEVELOPMENT.md`.
- **Implementation details for a single file?** Prefer JSDoc comments in the source code over prose in `DEVELOPMENT.md`.
- **Cross-cutting architecture or process?** Put it in `DEVELOPMENT.md` or the top-level `docs/` folder.

Follow the [Documentation Style Guide](DOCUMENTATION_STYLE.md) for JSDoc conventions, naming, and documentation structure.

## Codebase-Specific Reminders

1. **`index.d.ts` is hand-written** — always update it when changing the API surface
2. **`index.js` is more than a loader** — it contains significant logic (params, templates, events, disposal)
3. **Build release for benchmarks** — `npm run build:debug` produces binaries 10x+ slower
4. **`apache-arrow` is an optional peer dependency** — `arrow.mjs` gracefully handles its absence
5. **Tests use `.mjs` (ESM)** — use `import`/`export`, not `require` in test files
6. **CJS entry point** — `index.js` uses CommonJS (`require`/`module.exports`) for maximum compatibility
