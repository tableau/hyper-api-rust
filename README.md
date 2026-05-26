# Hyper API for Rust

[![CI](https://github.com/tableau/hyper-api-rust/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/tableau/hyper-api-rust/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/hyperdb-api.svg)](https://crates.io/crates/hyperdb-api)
[![docs.rs](https://img.shields.io/docsrs/hyperdb-api)](https://docs.rs/hyperdb-api)
[![Downloads](https://img.shields.io/crates/d/hyperdb-api.svg)](https://crates.io/crates/hyperdb-api)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A **pure-Rust** implementation of the Hyper database API, using the PostgreSQL
wire protocol with Hyper-specific extensions. Create, read, and manipulate Hyper
database files (`.hyper`) without any C library dependencies.

> **Project Status — 0.2.x, AI-Engineered**
>
> This crate was vibe-engineered with heavy use of AI coding assistants. The
> **0.2.x** line may still undergo large breaking changes; the public API
> won't settle until the 1.0.0 release.
>
> Contributors and reviewers should, at a minimum, run an **AI code reviewer**
> over any changes, following the conventions, layering rules, and patterns
> captured in [AGENTS.md](AGENTS.md) (and the subdirectory
> [`hyperdb-api-node/AGENTS.md`](hyperdb-api-node/AGENTS.md)). Those files are
> the authoritative guidance for AI assistants working in this repository.

## Key Features

- **Pure Rust** — no C library dependencies, standard `cargo build`
- **High Performance** — 22-24M rows/sec inserts, 18M rows/sec queries (100M row benchmark)
- **Memory Safe** — streaming by default, constant memory for billion-row results
- **Dual Architecture** — sync (`Connection`) and async (`AsyncConnection`) APIs
- **Connection Pooling** — async pooling via `deadpool` for high-concurrency applications
- **Arrow Integration** — insert and read data in Arrow IPC stream format
- **gRPC Transport** — read-only access with Arrow IPC and load balancing support
- **Full Type Support** — all Hyper types including Numeric, Geography, Intervals
- **Salesforce Auth** — OAuth 2.0 and JWT Bearer Token flows for Data Cloud
- **TLS** — via rustls (always-on, pure Rust)
- **Formal Verification** — Kani proof harnesses for model-checked correctness

## Quick Start

### Build from Source

Install Rust via [rustup.rs](https://rustup.rs/), install `protoc` for your
platform, download the `hyperd` executable with `make download-hyperd`
(bundled helper — see [hyperdb-bootstrap](hyperdb-bootstrap/README.md)), then
build:

| Platform | Install `protoc` | Build |
|----------|------------------|-------|
| **macOS** | `brew install protobuf` | `make build` |
| **Linux** (Debian/Ubuntu) | `sudo apt-get install -y protobuf-compiler build-essential` | `make build` |
| **Linux** (Fedora/RHEL) | `sudo dnf install protobuf-compiler` | `make build` |
| **Windows** | `choco install protoc` (also install [VS Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the "Desktop development with C++" workload for the MSVC linker) | `.\build.ps1 build` |

```bash
# Linux / macOS
make download-hyperd  # downloads hyperd into .hyperd/current/ (first time only)
make build            # or `make build-release` for optimized builds
make test             # runs unit + integration tests, test-release for release build
make doc              # builds the Hyper Rust documentation

# Windows (PowerShell)
.\build.ps1 download-hyperd
.\build.ps1 build     # or `.\build.ps1 build-release`
.\build.ps1 test      # or test-release for release build
.\build.ps1 doc       # builds the Hyper Rust documentation
```

The `Makefile` and `build.ps1` wrappers auto-discover the downloaded
`hyperd` at `.hyperd/current/hyperd`, and — if nothing is found on disk
— auto-run `download-hyperd` the first time you invoke a target that
actually needs `hyperd` (build, test, examples, doc). So `make test`
from a clean checkout Just Works; subsequent runs are cache hits. If
you already have a `hyperd` elsewhere, set `HYPERD_PATH=/path/to/hyperd`
and the downloader stays inert — nothing is fetched, and no build step
touches the network. Plain `cargo build` / `cargo test` also work as
long as one of those is true.

See [DEVELOPMENT.md](DEVELOPMENT.md#building--development) for the full
build guide including WSL, cross-compilation, benchmarks, and
per-platform troubleshooting.

### Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
hyperdb-api = { path = "hyperdb-api" }
```

#### Installing the CLIs

`hyperdb-mcp` and `hyperdb-bootstrap` ship two ways:

**Via npm** (recommended for `hyperdb-mcp`; bundles a matching `hyperd`):

```bash
npm install -g hyperdb-mcp
```

Supported platforms: macOS ARM64 (Apple Silicon), Linux x64 (glibc),
Windows x64. Intel macOS is built-from-source only at the moment — see
the platform table in [`hyperdb-mcp/README.md`](hyperdb-mcp/README.md#installation).

**Via crates.io** (compiles from source; no bundled `hyperd`):

```bash
cargo install hyperdb-mcp
cargo install hyperdb-bootstrap
```

`hyperdb-bootstrap` will then download a compatible `hyperd` for you:

```bash
hyperdb-bootstrap download
```

### Environment Setup

The `hyperd` executable (Hyper database server) must be available. The
simplest path is:

```bash
make download-hyperd   # or `.\build.ps1 download-hyperd` on Windows
```

This installs `hyperd` under `.hyperd/current/` in the repo and is
auto-discovered by the Makefile / `build.ps1`. If you already have a
`hyperd` elsewhere, export `HYPERD_PATH` instead:

```bash
export HYPERD_PATH=/path/to/hyperd
```

### Sync Example

```rust
use hyperdb_api::{
    Catalog, Connection, CreateMode, HyperProcess, Inserter,
    Result, SqlType, TableDefinition,
};

fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, "example.hyper", CreateMode::CreateIfNotExists)?;

    // Create a table
    let table_def = TableDefinition::from("users")
        .add_required_column("id", SqlType::int())
        .add_required_column("name", SqlType::text());
    Catalog::new(&conn).create_table(&table_def)?;

    // Insert data (COPY protocol, 22M+ rows/sec)
    {
        let mut inserter = Inserter::new(&conn, &table_def)?;
        inserter.add_row(&[&1i32, &"Alice"])?;
        inserter.add_row(&[&2i32, &"Bob"])?;
        inserter.execute()?;
    }

    // Query data
    let result = conn.execute_query("SELECT * FROM users")?;
    for row in result.rows() {
        let row = row?;
        let id: Option<i32> = row.get(0);
        let name: Option<String> = row.get(1);
        println!("{:?} - {:?}", id, name);
    }

    Ok(())
}
```

### Async Example

```rust
use hyperdb_api::{AsyncConnection, CreateMode, HyperProcess, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let endpoint = hyper.require_endpoint()?;

    let conn = AsyncConnection::connect(
        endpoint,
        "example_async.hyper",
        CreateMode::CreateIfNotExists,
    ).await?;

    conn.execute_command("CREATE TABLE users (id INT, name TEXT)").await?;
    conn.execute_command("INSERT INTO users VALUES (1, 'Alice')").await?;
    conn.close().await?;
    Ok(())
}
```

## Crate Overview

| Crate | Purpose | Published |
|-------|---------|-----------|
| **[hyperdb-api](hyperdb-api/README.md)** | High-level API — connections, inserters, catalog, Arrow, pooling | crates.io |
| **[hyperdb-api-core](hyperdb-api-core/README.md)** | Internal implementation details (types, protocol, client). Not a public API — depend on `hyperdb-api` instead. | crates.io |
| **[hyperdb-api-salesforce](hyperdb-api-salesforce/README.md)** | Salesforce Data Cloud OAuth authentication | crates.io |
| **[hyperdb-mcp](hyperdb-mcp/README.md)** | MCP server for LLM-driven SQL analytics on `.hyper` files | crates.io |
| **[sea-query-hyperdb](sea-query-hyperdb/README.md)** | HyperDB dialect backend for sea-query | crates.io |
| **[hyperdb-api-node](hyperdb-api-node/README.md)** | Node.js/TypeScript bindings via napi-rs | npm |
| **[hyperdb-bootstrap](hyperdb-bootstrap/README.md)** | Download the `hyperd` executable from Tableau's release packages | crates.io |

## Examples

The API ships 14 examples in `hyperdb-api/examples/` plus 2 companion crate examples.

### Core Examples

| Example | Description |
|---------|-------------|
| `insert_data_into_single_table` | Create a table and insert data using `Inserter` |
| `insert_data_into_multiple_tables` | Multiple related tables |
| `create_hyper_file_from_csv` | Load CSV data into a Hyper table |
| `delete_data_in_existing_hyper_file` | Delete data with SQL `DELETE` |
| `update_data_in_existing_hyper_file` | Update data with SQL `UPDATE` |
| `read_and_print_data_from_existing_hyper_file` | Read table definitions and query data |
| `insert_data_with_expressions` | Column mappings with `MappedInserter` |
| `insert_geospatial_data_to_a_hyper_file` | Insert geospatial data |

### Rust-Specific Examples

| Example | Description |
|---------|-------------|
| `arrow` | Read/write Arrow `RecordBatch` data |
| `async_usage` | `AsyncConnection` and Tokio patterns |
| `threaded_inserter` | Multi-threaded bulk insertion with `InsertChunk`/`ChunkSender` |
| `grpc_query` | gRPC transport, Arrow IPC results |
| `connection_pool` | Async connection pooling with deadpool |
| `transactions` | RAII guards, multi-table rollback, DDL, reconnect semantics |

### Running Examples

```bash
export HYPERD_PATH=/path/to/hyperd

# Run individual examples
cargo run -p hyperdb-api --example insert_data_into_single_table
cargo run -p hyperdb-api --example arrow
cargo run -p hyperdb-api --example connection_pool

# Companion crate examples
cargo run -p sea-query-hyperdb --example basic_usage
cargo run -p hyperdb-api-salesforce --example salesforce_auth_example

# Run all examples
./run_all_examples.sh
```

## Companion Crates

### sea-query-hyperdb

HyperDB dialect backend for [sea-query](https://crates.io/crates/sea-query) — use for
window functions, CTEs, complex JOINs, and type-safe query composition:

```toml
[dependencies]
sea-query = "0.32"
sea-query-hyperdb = { path = "sea-query-hyperdb" }
```

```rust
use sea_query::{Query, Expr, Iden};
use sea_query_hyperdb::HyperQueryBuilder;

let sql = Query::select()
    .column(Users::Name)
    .from(Users::Table)
    .and_where(Expr::col(Users::Age).gt(18))
    .to_string(HyperQueryBuilder);

let result = conn.fetch_all(&sql)?;
```

### hyperdb-api-salesforce

Salesforce Data Cloud OAuth authentication — JWT Bearer Token, Username-Password,
and Refresh Token flows:

```toml
[dependencies]
hyperdb-api-salesforce = { path = "hyperdb-api-salesforce" }
```

```rust
use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};

let auth_config = SalesforceAuthConfig::new(
    "https://login.salesforce.com",
    "your-connected-app-consumer-key",
)?
.auth_mode(AuthMode::private_key("user@example.com", &private_key_pem)?);

let token_provider = SharedTokenProvider::new(auth_config)?;
```

See [hyperdb-api-salesforce/README.md](hyperdb-api-salesforce/README.md) for full setup guide.

## Node.js Bindings

The `hyperdb-api-node` package provides Node.js and TypeScript bindings built with
[napi-rs](https://napi.rs/):

```typescript
const { HyperProcess, Connection, CreateMode } = require('hyperdb-api-node');

const hyper = new HyperProcess();
const conn = await Connection.connect(hyper.endpoint, 'my.hyper', CreateMode.CreateAndReplace);

// Tagged template literals — SQL injection safe
const rows = await conn.sql`SELECT * FROM users WHERE age > ${18}`;
await conn.close();
hyper.close();
```

See [hyperdb-api-node/README.md](hyperdb-api-node/README.md) for full documentation.

## Platform Support

| Platform | Status | Build Tool |
|----------|--------|------------|
| Linux (x86_64) | Supported | `make build` |
| macOS (ARM & x64) | Supported | `make build` |
| Windows | Supported | `.\build.ps1 build` |
| WSL | Supported | `make build` |

**MSRV:** Check `rust-version` in `Cargo.toml`.

## Documentation

| Resource | Description |
|----------|-------------|
| **[hyperdb-api/README.md](hyperdb-api/README.md)** | Full user guide for the `hyperdb-api` crate |
| **[DEVELOPMENT.md](DEVELOPMENT.md)** | Architecture, building, testing, benchmarks — for contributors |
| **[CONTRIBUTING.md](CONTRIBUTING.md)** | How to contribute |
| **[docs/TRANSACTIONS.md](docs/TRANSACTIONS.md)** | Transaction API design |
| **[docs/BENCHMARK_GUIDE.md](docs/BENCHMARK_GUIDE.md)** | How to run benchmarks |

Per-crate documentation: each crate has its own `README.md` (see [Crate Overview](#crate-overview)).

Generate API docs locally:

```bash
make doc    # or: cargo doc --no-deps --open
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the governance model, contribution checklist,
commit message format, and pull request process.

## Acknowledgments

This project includes code adapted from
[sfackler/rust-postgres](https://github.com/sfackler/rust-postgres) (the
`postgres-protocol`, `tokio-postgres`, and `postgres-types` crates by Steven
Fackler, MIT or Apache-2.0). See [`NOTICE`](NOTICE) for the full third-party
attribution list and the upstream license text.

## License

Licensed under either of [MIT](LICENSE-MIT.txt) or [Apache-2.0](LICENSE-APACHE.txt) at your option.
