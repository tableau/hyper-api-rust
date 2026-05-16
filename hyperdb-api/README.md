# hyperdb-api

A **pure-Rust** implementation of the Hyper database API. Create, read, and manipulate
Hyper database files (`.hyper`) without any C library dependencies.

- 22-24M rows/sec inserts, 18M rows/sec queries (100M row benchmark)
- Streaming by default — constant memory for billion-row results
- Both sync (`Connection`) and async (`AsyncConnection`) APIs
- No feature flags — everything is always available

## Installation

```toml
[dependencies]
hyperdb-api = "0.1"
```

## Runtime Requirements

The `hyperd` executable (Hyper database server) must be available. Set its path via:

```bash
export HYPERD_PATH=/path/to/hyperd
```

## Quick Start

```rust
use hyperdb_api::{
    Catalog, Connection, CreateMode, HyperProcess, Inserter,
    Result, SqlType, TableDefinition,
};

fn main() -> Result<()> {
    // Start a Hyper server
    let hyper = HyperProcess::new(None, None)?;

    // Connect to a database
    let conn = Connection::new(&hyper, "example.hyper", CreateMode::CreateIfNotExists)?;

    // Create a table
    let table_def = TableDefinition::from("users")
        .add_required_column("id", SqlType::int())
        .add_required_column("name", SqlType::text());
    Catalog::new(&conn).create_table(&table_def)?;

    // Insert data using the high-performance Inserter (COPY protocol)
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
        println!("User: {:?} - {:?}", id, name);
    }

    Ok(())
}
```

## Async Quick Start

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

## Connection Pooling

For high-concurrency async applications, use the built-in connection pool:

```rust
use hyperdb_api::pool::{create_pool, PoolConfig};
use hyperdb_api::{CreateMode, HyperProcess, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let endpoint = hyper.require_endpoint()?;

    let config = PoolConfig::new(&endpoint, "pooled.hyper")
        .create_mode(CreateMode::CreateIfNotExists)
        .max_size(10);

    let pool = create_pool(config)?;

    // Get connections from the pool — returned automatically when dropped
    let conn = pool.get().await.map_err(|e| hyperdb_api::Error::new(e.to_string()))?;
    conn.execute_command("SELECT 1").await?;

    Ok(())
}
```

| Option | Default | Description |
|--------|---------|-------------|
| `max_size(n)` | 16 | Maximum connections in pool |
| `create_mode(mode)` | `DoNotCreate` | Database creation mode |
| `auth(user, pass)` | None | Authentication credentials |

## gRPC Transport

The API supports gRPC as an alternative to TCP for read-only queries with Arrow IPC
results. The unified `Connection` type auto-detects transport based on URL:

```rust
use hyperdb_api::{Connection, CreateMode};

// http:// or https:// → gRPC (auto-detected)
let conn = Connection::connect(
    "http://localhost:7484",
    "my_database.hyper",
    CreateMode::DoNotCreate
)?;

// Same query API as TCP
let mut result = conn.execute_query("SELECT * FROM users")?;
```

Start Hyper with gRPC enabled:

```rust
use hyperdb_api::{HyperProcess, ListenMode, Parameters};

let mut params = Parameters::new();
params.set_listen_mode(ListenMode::Both { grpc_port: 7484 });
let hyper = HyperProcess::new(None, Some(&params))?;
```

| Transfer Mode | Best For |
|---------------|----------|
| `SYNC` | Small results |
| `ASYNC` | Very large results |
| `ADAPTIVE` (default) | Most workloads |

## Query Execution

### Convenience Methods

```rust
// Fetch single row (error if no rows)
let user = conn.fetch_one("SELECT * FROM users WHERE id = 1")?;

// Fetch optional row (None if no rows)
let user = conn.fetch_optional("SELECT * FROM users WHERE id = 999")?;

// Fetch all rows
let users = conn.fetch_all("SELECT * FROM users")?;

// Fetch scalar value
let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM users")?;
```

### Parameterized Queries

Always use parameterized queries with user input to prevent SQL injection:

```rust
let mut result = conn.query_params(
    "SELECT * FROM users WHERE name = $1 AND age >= $2",
    &[&"Alice", &30i32],
)?;

// For INSERT/UPDATE/DELETE:
let rows_affected = conn.command_params(
    "UPDATE users SET balance = $1 WHERE name = $2",
    &[&750.0f64, &"Alice"],
)?;
```

Supported parameter types: `i16`, `i32`, `i64`, `f32`, `f64`, `bool`, `&str`, `String`,
`Option<T>`, `Date`, `Time`, `Timestamp`, `OffsetTimestamp`, `Vec<u8>`, `&[u8]`.

### Streaming Results

Results are always streaming with constant memory usage:

```rust
// Chunked iteration (batch processing, maximum performance)
let mut result = conn.execute_query("SELECT * FROM large_table")?;
while let Some(chunk) = result.next_chunk()? {
    for row in &chunk {
        let id: Option<i32> = row.get(0);
    }
}

// Row iterator (simpler, slightly more overhead)
let result = conn.execute_query("SELECT * FROM large_table")?;
for row in result.rows() {
    let row = row?;
    let id: Option<i32> = row.get(0);
}
```

## Bulk Data Insertion

### Inserter (COPY Protocol)

The high-performance `Inserter` uses HyperBinary COPY protocol for 22M+ rows/sec:

```rust
let mut inserter = Inserter::new(&conn, &table_def)?;
inserter.add_row(&[&1i32, &"Widget", &9.99f64])?;
inserter.add_row(&[&2i32, &None::<&str>, &19.99f64])?;  // NULL name
inserter.execute()?;
```

Type-specific methods are also available (similar to the C++ API):

```rust
inserter.add_i32(1)?;
inserter.add_str("Widget")?;
inserter.add_f64(9.99)?;
inserter.end_row()?;
```

### Column Mappings

Insert data with computed columns using `MappedInserter`:

```rust
let mappings = vec![
    ColumnMapping::new("id"),
    ColumnMapping::new("quantity"),
    ColumnMapping::with_expression("total", "\"quantity\" * \"unit_price\""),
];

let mut inserter = Inserter::with_column_mappings(&conn, &inserter_def, "orders", &mappings)?;
```

### Arrow Format

Insert pre-formatted Arrow IPC data:

```rust
let mut inserter = ArrowInserter::new(&conn, &table_def)?;
inserter.insert_data(&arrow_ipc_data)?;
let rows = inserter.execute()?;
```

### Multi-threaded Insertion

For maximum throughput on multi-core systems, use `InsertChunk` and `ChunkSender` to
parallelize data encoding:

```rust
let sender = ChunkSender::new(&conn, &table_def)?;

// Workers encode chunks in parallel using InsertChunk
let chunk = InsertChunk::from_table_definition(&table_def);
// ... populate chunk ...
sender.send_chunk(chunk)?;

let total_rows = sender.finish()?;
```

## Catalog Operations

```rust
let catalog = Catalog::new(&conn);

// List schemas and tables
let schemas = catalog.get_schema_names::<&str>(None)?;
let tables = catalog.get_table_names("public")?;

// Check existence
if catalog.has_table("public.users")? {
    let table_def = catalog.get_table_definition("public.users")?;
    println!("Columns: {}", table_def.column_count());
}
```

## SQL-Safe Names

Type-safe SQL identifier handling with automatic escaping:

```rust
use hyperdb_api::{Name, TableName};

// Simple construction
let name = Name::try_new("users")?;

// Qualified names (dot-separated parsing)
let table: TableName = "mydb.public.users".parse()?;

// Fluent builder
let table = TableName::try_new("users")?.with_schema("public")?;

// Macros
let table = table_name!("public", "users")?;
```

Most API methods accept strings directly via `impl TryInto<T>`:

```rust
// No manual conversion needed
if catalog.has_table("public.users")? { /* ... */ }
```

## Transactions

```rust
let txn = conn.transaction()?;
txn.execute_command("INSERT INTO users VALUES (1, 'Alice')")?;
txn.execute_command("INSERT INTO users VALUES (2, 'Bob')")?;
txn.commit()?;  // both inserts are committed; drop without commit() auto-rolls-back
```

See [docs/TRANSACTIONS.md](../docs/TRANSACTIONS.md) for full details.

## Connection Features

### Query Cancellation

Thread-safe cancellation from another thread:

```rust
let conn = Arc::new(Connection::create_or_open(&hyper, "test.hyper")?);
// ... in another thread:
conn.cancel()?;  // Cancels running query (SQLSTATE 57014)
```

### Notice Receiver

```rust
conn.set_notice_receiver(Some(Box::new(|notice| {
    println!("Notice: {} ({})", notice.message, notice.severity.as_deref().unwrap_or(""));
})));
```

### Query Statistics

Per-query performance metrics from Hyper's internal log:

```rust
use hyperdb_api::query_stats::LogFileStatsProvider;

conn.enable_query_stats(LogFileStatsProvider::from_process(&hyper));
conn.execute_command("SELECT * FROM users")?;

if let Some(stats) = conn.last_query_stats() {
    println!("Elapsed: {:.3}ms", stats.elapsed_s * 1000.0);
}
```

### Logging

The API uses the `tracing` crate for structured logging:

```bash
# Enable via environment variable
RUST_LOG=hyperdb_api=info cargo run

# Debug-level for auth details and chunk progress
RUST_LOG=hyperdb_api=debug cargo run
```

## Examples

| Example | Description | Command |
|---------|-------------|---------|
| `insert_data_into_single_table` | Create table and insert data | `cargo run -p hyperdb-api --example insert_data_into_single_table` |
| `insert_data_into_multiple_tables` | Multiple related tables | `cargo run -p hyperdb-api --example insert_data_into_multiple_tables` |
| `create_hyper_file_from_csv` | Load CSV into Hyper | `cargo run -p hyperdb-api --example create_hyper_file_from_csv` |
| `read_and_print_data_from_existing_hyper_file` | Read and query data | `cargo run -p hyperdb-api --example read_and_print_data_from_existing_hyper_file` |
| `insert_data_with_expressions` | Column mappings | `cargo run -p hyperdb-api --example insert_data_with_expressions` |
| `insert_geospatial_data_to_a_hyper_file` | Geography column | `cargo run -p hyperdb-api --example insert_geospatial_data_to_a_hyper_file` |
| `arrow` | Arrow RecordBatch read/write | `cargo run -p hyperdb-api --example arrow` |
| `async_usage` | AsyncConnection + Tokio | `cargo run -p hyperdb-api --example async_usage` |
| `threaded_inserter` | Multi-threaded bulk insert | `cargo run -p hyperdb-api --example threaded_inserter` |
| `grpc_query` | gRPC transport + Arrow IPC | `cargo run -p hyperdb-api --example grpc_query` |
| `connection_pool` | Async connection pooling | `cargo run -p hyperdb-api --example connection_pool` |
| `transactions` | RAII guards, multi-table rollback, DDL, reconnect semantics | `cargo run -p hyperdb-api --example transactions` |

Run all examples:

```bash
export HYPERD_PATH=/path/to/hyperd
./run_all_examples.sh
```

## Companion Crates

### sea-query-hyperdb

HyperDB dialect backend for [sea-query](https://crates.io/crates/sea-query) — use for
window functions, CTEs, complex JOINs, and type-safe query composition.

```toml
[dependencies]
sea-query = "0.32"
sea-query-hyperdb = "0.1"
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
and Refresh Token flows.

```toml
[dependencies]
hyperdb-api-salesforce = "0.1"
```

See [hyperdb-api-salesforce/README.md](../hyperdb-api-salesforce/README.md) for setup instructions.

## Acknowledgments

This crate depends on `hyperdb-api-core`, which includes code adapted from
[sfackler/rust-postgres](https://github.com/sfackler/rust-postgres) (the
`postgres-protocol`, `tokio-postgres`, and `postgres-types` crates by Steven
Fackler, MIT or Apache-2.0). See the
[`NOTICE`](https://github.com/tableau/hyper-api-rust/blob/main/NOTICE) file at
the workspace root for the full third-party attribution list. (Relative links
to `NOTICE` work on GitHub but not on crates.io; the absolute URL above is
crates.io-friendly.)

## License

Apache-2.0
