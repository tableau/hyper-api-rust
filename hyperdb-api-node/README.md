# hyperdb-api-node

Node.js bindings for the [Hyper database](https://tableau.github.io/hyper-db/) API, powered by [napi-rs](https://napi.rs/).

This package provides a native Node.js addon that gives JavaScript and TypeScript developers high-performance access to Hyper database files (`.hyper`) using the pure Rust `hyperdb-api` crate under the hood.

## Features

- **Async-first** — all I/O methods return Promises
- **Tagged template literals** — `conn.sql\`SELECT * FROM t WHERE id = ${id}\`` — safe by default
- **Apache Arrow integration** — query results as Arrow Tables at 15M+ rows/sec
- **Native performance** — compiled Rust, no FFI overhead at runtime
- **Full TypeScript support** — hand-written `.d.ts` declarations with full IntelliSense
- **Connection pooling** — configurable min/max, idle timeout, acquire timeout
- **Multiple query APIs** — row-oriented, columnar, Arrow IPC, tagged templates
- **Resource management** — `Symbol.asyncDispose` for `await using` (Node 22+)
- **Query tracing** — `conn.on('query', ...)` hooks for logging/metrics
- **Cross-platform** — macOS (ARM & x64), Linux, Windows

## Class Overview

```
HyperProcess ──creates──▶ Connection ──returns──▶ RowData
ConnectionBuilder ─builds─▶ Connection ──returns──▶ QueryStream ──yields──▶ RowData
ConnectionPool ──pools──▶ Connection ──returns──▶ ColumnarStream ──yields──▶ ColumnarChunk

Catalog ──uses──▶ Connection          TableDefinition ──uses──▶ SqlType
Inserter ──uses──▶ Connection + TableDefinition
```

For a detailed class diagram with all methods and properties, see
[DEVELOPMENT.md](DEVELOPMENT.md#class-diagram).

## Requirements

- **Node.js** >= 21
- **Hyper server binary** (`hyperd`) — set `HYPERD_PATH` env var or place in standard location

## Installation

```bash
npm install hyperdb-api-node
```

npm will automatically install the correct prebuilt binary for your platform via `optionalDependencies` — no Rust toolchain required.

**Supported platforms:**

| Platform | Architecture | Package |
|---|---|---|
| macOS | ARM64 (Apple Silicon) | `hyperdb-api-node-darwin-arm64` |
| Linux | x64 (glibc) | `hyperdb-api-node-linux-x64-gnu` |
| Linux | x64 (musl/Alpine) | `hyperdb-api-node-linux-x64-musl` |
| Linux | ARM64 (glibc) | `hyperdb-api-node-linux-arm64-gnu` |
| Windows | x64 (MSVC) | `hyperdb-api-node-win32-x64-msvc` |

> **macOS x64 (Intel) is not currently published** while we wait for `macos-13`
> GitHub Actions runners to become reliably available. Build from source for
> Intel Macs in the meantime.

### Build from source

To build from the workspace source instead, you'll need:

- Node.js >= 21
- Rust stable (install via [rustup](https://rustup.rs/))
- `protoc` — `brew install protobuf` (macOS), `apt-get install protobuf-compiler` (Debian/Ubuntu), or `choco install protoc` (Windows)
- The `hyperd` binary (`make download-hyperd` from the repo root, or set `HYPERD_PATH` to an existing copy)

```bash
git clone <repo-url>
cd hyper-api-rust/hyperdb-api-node
npm install
npm run build       # release build; use `npm run build:debug` for faster iteration
```

This produces `hyperdb-api-node.<platform-tag>.node` next to `index.js`. To use it from another local project:

```bash
cd ../my-app
npm install ../hyperdb-api-node      # file: dependency
```

See [DEVELOPMENT.md](DEVELOPMENT.md) for cross-compilation, prebuild orchestration, npm publishing flow, and troubleshooting.

## Quick Start

### TypeScript (.mts)

```typescript
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const {
  HyperProcess, Connection, CreateMode, Catalog,
  TableDefinition, SqlType, Inserter,
} = require('hyperdb-api-node') as typeof import('hyperdb-api-node');
import type { RowData } from 'hyperdb-api-node';

const hyper = new HyperProcess();
const conn = await Connection.connect(hyper.endpoint, 'example.hyper', CreateMode.CreateIfNotExists);

// Define and create a table
const tableDef = new TableDefinition('users');
tableDef.addColumn('id', SqlType.int(), false);       // NOT NULL
tableDef.addColumn('name', SqlType.text(), true);      // NULLABLE
tableDef.addColumn('score', SqlType.double(), true);
await new Catalog(conn).createTable(tableDef);

// Insert data
const inserter = new Inserter(conn, tableDef);
inserter.addRows([[1, 'Alice', 95.5], [2, 'Bob', null], [3, 'Charlie', 88.0]]);
console.log(`Inserted ${await inserter.execute()} rows`);

// Query with tagged template (safe by default)
const rows: RowData[] = await conn.sql`SELECT * FROM users WHERE score > ${80}`;
for (const row of rows) {
  console.log(`id=${row.getInt32(0)}, name=${row.getString(1)}, score=${row.getFloat64(2)}`);
}

await conn.close();
hyper.close();
```

### JavaScript (.mjs)

The same code works in plain JavaScript -- just change the `require()` call:

```javascript
const { HyperProcess, Connection, CreateMode, /* ... */ } = require('hyperdb-api-node');
```

See `examples/complete-api-tour.mjs` for a full JavaScript walkthrough.

## API Reference

### `HyperProcess`

Manages a local Hyper server process (`hyperd`).

```typescript
const hyper = new HyperProcess();             // auto-detect hyperd location
const hyper = new HyperProcess('/path/to/hyperd'); // or specify path

hyper.endpoint;  // e.g., "localhost:7483"
hyper.isOpen;    // true

// Convenience: connect directly
const conn = await hyper.connectToDatabase('test.hyper', CreateMode.CreateIfNotExists);

hyper.close();   // MUST be called when done
```

### `Connection`

An async connection to a Hyper database. All I/O methods return Promises.

```typescript
// Connect to a database
const conn = await Connection.connect('localhost:7483', 'db.hyper', CreateMode.CreateIfNotExists);

// With authentication
const conn = await Connection.connectWithAuth(endpoint, 'db.hyper', CreateMode.CreateIfNotExists, 'user', 'pass');

// Without a database (server-level operations)
const conn = await Connection.withoutDatabase('localhost:7483');

// Execute commands (returns affected row count) and queries (returns RowData[])
const affected = await conn.executeCommand("INSERT INTO users VALUES (1, 'Alice')");
const rows = await conn.executeQuery('SELECT * FROM users');
const schema = await conn.querySchema('SELECT * FROM users');
// schema = [{ name: 'id', typeName: 'INTEGER', index: 0 }, ...]

conn.database;  // 'db.hyper' or null
conn.isAlive;   // true
await conn.close(); // MUST be called when done
```

### `ConnectionBuilder`

Fluent builder for connections with authentication and timeouts. Transport is
auto-detected: `localhost:7483` -> TCP, `https://server:443` -> gRPC.

```typescript
const conn = await new ConnectionBuilder('localhost:7483')
  .database('my.hyper')
  .createMode(CreateMode.CreateIfNotExists)
  .user('admin').password('secret')
  .loginTimeout(5000)
  .build();
```

### `CreateMode`

Controls database creation behavior when connecting.

```typescript
CreateMode.DoNotCreate       // Database must exist
CreateMode.Create            // Create new (fails if exists)
CreateMode.CreateIfNotExists // Create if missing
CreateMode.CreateAndReplace  // Drop and recreate
```

### `RowData`

A row from a query result. Access values by column index (0-based).

```typescript
const rows = await conn.executeQuery('SELECT id, name, active FROM users');

for (const row of rows) {
  // Typed accessors (return null for NULL values or type mismatches)
  row.getInt32(0);    // number | null
  row.getInt64(1);    // number | null (caution: precision loss above 2^53)
  row.getFloat64(2);  // number | null
  row.getString(3);   // string | null (converts non-string types to string)
  row.getBool(4);     // boolean | null
  row.getBytes(5);    // Buffer | null

  // Metadata
  row.isNull(0);      // boolean
  row.columnCount;    // number
}
```

### `SqlType`

SQL column types. Use static factory methods to create instances.

```typescript
// Numeric
SqlType.bool()                SqlType.smallInt()           SqlType.int()
SqlType.bigInt()              SqlType.float()              SqlType.double()
SqlType.numeric(18, 2)        // NUMERIC(precision, scale)

// String / Binary
SqlType.text()                SqlType.varchar(255)         SqlType.char(10)
SqlType.bytes()               // BYTEA

// Date / Time
SqlType.date()                SqlType.time()               SqlType.timestamp()
SqlType.timestampTz()         SqlType.interval()

// Other
SqlType.json()                SqlType.geography()

SqlType.numeric(18, 2).toString(); // "NUMERIC(18, 2)"
```

### `TableDefinition`

Defines a table schema for creating tables or bulk inserts.

```typescript
const tableDef = new TableDefinition('products');
tableDef.withSchema('inventory');
tableDef.addColumn('id', SqlType.int(), false);          // NOT NULL
tableDef.addColumn('name', SqlType.text(), true);         // NULLABLE
tableDef.addColumn('price', SqlType.numeric(10, 2), true);

tableDef.name;         // 'products'
tableDef.schema;       // 'inventory'
tableDef.columnCount;  // 3
tableDef.getColumns(); // [{ name: 'id', typeName: 'INTEGER', nullable: false }, ...]
tableDef.toCreateSql(); // 'CREATE TABLE "inventory"."products" (...)'
```

### `Catalog`

Database catalog operations: create/drop tables and schemas.

```typescript
const catalog = new Catalog(conn);

// Schemas
await catalog.createSchema('analytics');
await catalog.getSchemaNames();              // ['public', 'analytics']
await catalog.dropSchema('analytics', true); // cascade=true

// Tables
await catalog.createTable(tableDef);
await catalog.hasTable('public.users');      // true
await catalog.getTableNames('public');       // ['users', 'products']
await catalog.dropTable('public.users');
```

### `Inserter`

High-performance bulk data inserter using the COPY protocol.

```typescript
const inserter = new Inserter(conn, tableDef);

inserter.addRow([1, 'Widget', 19.99]);          // one at a time
inserter.addRows([                               // or batch
  [2, 'Gadget', null],
  [3, 'Doohickey', 5.99],
]);

console.log(inserter.bufferedRowCount);          // 3
const count = await inserter.execute();          // send to server, returns row count

// Inserter can be reused after execute()
inserter.addRow([4, 'Another', 7.77]);
await inserter.execute();
```

### `QueryStream`

Streaming query results for memory-efficient iteration over large result sets.
Unlike `executeQuery()` which loads all rows into memory, `QueryStream` fetches
rows in chunks (~64K rows each).

```typescript
// Option A: chunk-level iteration (high performance, batch processing)
const stream = conn.executeQueryStream('SELECT * FROM big_table');
let chunk;
while ((chunk = await stream.nextChunk()) !== null) {
  for (const row of chunk) { row.getInt32(0); }
}

// Option B: row-level async iteration (convenient)
for await (const row of conn.executeQueryStream('SELECT * FROM big_table')) {
  console.log(row.getInt32(0), row.getString(1));
}

// Column metadata is available via stream.getSchema() after the first chunk
```

### Type Mappings

**Reading (Hyper -> JS):**

| SQL Type | Method | JS Type |
|---|---|---|
| INT | `getInt32(i)` | `number` |
| BIGINT | `getInt64(i)` / `getBigInt(i)` | `number` / `bigint` |
| DOUBLE | `getFloat64(i)` | `number` |
| BOOLEAN | `getBool(i)` | `boolean` |
| TEXT / VARCHAR | `getString(i)` | `string` |
| BYTEA | `getBytes(i)` | `Buffer` |
| DATE | `getDateMs(i)` -> `new Date(ms)` | `number` (Unix ms) |
| TIMESTAMP / TZ | `getTimestampMs(i)` -> `new Date(ms)` | `number` (Unix ms) |
| JSON | `getJson(i)` -> `JSON.parse()` | `string` |
| Any type | `getString(i)` | `string` (fallback) |

**Writing (JS -> Hyper) — via Inserter or tagged templates:**

| JS Type | SQL Type |
|---|---|
| `number` | INT / BIGINT / DOUBLE (auto-detected) |
| `string` | TEXT |
| `boolean` | BOOLEAN |
| `null` / `undefined` | NULL |
| `Buffer` | BYTEA |
| `Date` | TIMESTAMP (via `toISOString()`) |
| `{}` (plain object) | JSON (via `JSON.stringify()`) |

### Tagged Template Literals

The idiomatic way to write safe queries. Values are automatically escaped to SQL literals -- no SQL injection possible:

```js
const rows = await conn.sql`SELECT * FROM users WHERE name = ${name} AND age > ${minAge}`;
await conn.command`INSERT INTO users (id, name) VALUES (${id}, ${name})`;
await conn.command`UPDATE users SET score = ${score} WHERE id = ${id}`;
```

Supported value types: `number`, `string`, `boolean`, `null`, `Buffer`.

### Parameterized Queries

`$1`/`$2` placeholder syntax (alternative to tagged templates):

```typescript
const rows = await conn.executeQueryParams(
  'SELECT * FROM users WHERE name = $1 AND age > $2', ['Alice', 30]
);
await conn.executeCommandParams(
  'INSERT INTO users (id, name, score) VALUES ($1, $2, $3)', [1, 'Alice', 95.5]
);
```

Supported parameter types: `number`, `string`, `boolean`, `null`, `Buffer`. Values are escaped -- no SQL injection possible.

### `ConnectionPool`

A connection pool that manages reusable database connections.
Import from `hyperdb-api-node/pool.mjs`.

```js
import { ConnectionPool } from 'hyperdb-api-node/pool.mjs';

const pool = new ConnectionPool(hyper.endpoint, 'data.hyper', {
  min: 2, max: 10, idleTimeoutMs: 30_000,
});

// Shorthand: auto acquire/release
const rows = await pool.query('SELECT * FROM users');
const users = await pool.queryParams('SELECT * FROM users WHERE age > $1', [18]);

// Custom logic with auto acquire/release
const result = await pool.use(async (conn) => {
  await conn.executeCommand('BEGIN');
  await conn.executeCommandParams('INSERT INTO logs VALUES ($1)', ['event']);
  await conn.executeCommand('COMMIT');
  return 'ok';
});

// Pool stats: pool.size, pool.idle, pool.active, pool.pending
await pool.close();
```

| Option | Default | Description |
|---|---|---|
| `min` | 0 | Minimum idle connections to keep alive |
| `max` | 10 | Maximum total connections |
| `idleTimeoutMs` | 30000 | Close idle connections after this many ms |
| `acquireTimeoutMs` | 30000 | Max ms to wait for a connection (0 = no limit) |
| `createMode` | `CreateIfNotExists` | Database creation mode |

### `RowData` Extras

```js
row.getBigInt(0);            // lossless 64-bit as BigInt (e.g., 9007199254740993n)
row.toJSON();                // { "0": "1", "1": "Alice", "2": "95.5" }

// Schema-aware keys
row.setColumnNames(schema.map(c => c.name));
row.toJSON();                // { id: "1", name: "Alice", score: "95.5" }
```

### Query Event Hooks

```js
conn.on('query', ({ sql, durationMs, rowCount, type }) => {
  console.log(`[${type}] ${sql} — ${durationMs}ms, ${rowCount} rows`);
});
conn.off('query', listener); // unsubscribe
```

### Resource Management (`await using`)

On Node.js 22+, `Connection` supports `Symbol.asyncDispose` and `HyperProcess` supports `Symbol.dispose`:

```ts
{
  await using conn = await Connection.connect(endpoint, db, CreateMode.CreateIfNotExists);
  await conn.sql`INSERT INTO t VALUES (${1}, ${'Alice'})`;
} // conn.close() called automatically

{ using hyper = new HyperProcess(); /* ... */ } // hyper.close() called automatically
```

## Error Handling

All async methods throw `Error` on failure with details from the Rust/Hyper layer:

```typescript
try { await conn.executeCommand('SELECT * FROM nonexistent'); }
catch (err) { console.error(err.message); }
```

## Apache Arrow Integration

hyperdb-api-node has first-class [Apache Arrow](https://arrow.apache.org/) support.
Hyper serializes query results directly to Arrow IPC format — no intermediate
row/column conversion — making this the **fastest data extraction path**.

```bash
# apache-arrow is an optional peer dependency
npm install apache-arrow
```

### Raw IPC Buffers (zero-dependency)

`executeQueryToArrow()` and `exportTableToArrow()` return raw Arrow IPC bytes
as a `Buffer`. No `apache-arrow` package needed:

```js
const buf = await conn.executeQueryToArrow('SELECT * FROM measurements');
writeFileSync('results.arrows', buf); // readable by DuckDB, Polars, pandas
```

### Using `arrow.mjs` (with `apache-arrow`)

The convenience module wraps IPC buffers into full Arrow Tables:

```js
import { tableFromQuery, exportTable, queryToArrowFile, insertFromTable } from 'hyperdb-api-node/arrow.mjs';

// Query -> Arrow Table
const table = await tableFromQuery(conn, 'SELECT * FROM sales');
const revenue = table.getChild('revenue').toArray();  // Float64Array

// Export to .arrow file (IPC file format, readable by DuckDB/Polars/pandas/R)
const bytes = await queryToArrowFile(conn, 'SELECT * FROM sales');
writeFileSync('sales.arrow', bytes);

// Insert from Arrow Table
const count = await insertFromTable(conn, tableDef, arrowTable);
```

| Function | Description |
|---|---|
| `tableFromQuery(conn, sql)` | Query -> Arrow `Table` |
| `exportTable(conn, tableName)` | Export table -> Arrow `Table` |
| `batchesFromQuery(conn, sql)` | Query -> `RecordBatch[]` |
| `queryToArrowFile(conn, sql)` | Query -> Arrow IPC file bytes (`.arrow`) |
| `insertFromTable(conn, def, table)` | Arrow `Table` -> Hyper insert |
| `querySchema(conn, sql)` | Query -> Arrow `Schema` (no data) |

## Benchmark Results

Measured on **Apple M3 Max**, Node.js v24, release build, 1M rows.

Table schema: `measurements(id INT NOT NULL, sensor_id INT, value DOUBLE, timestamp BIGINT)` — 24 bytes/row.

### Query Performance

| Benchmark | Rows/sec | MB/s | Notes |
|---|---|---|---|
| **Arrow Full Scan** | 15.5M | 355.4 | `executeQueryToArrow` + `tableFromIPC` — fastest |
| **Arrow Filtered** | 15.6M | 358.1 | Arrow IPC with `WHERE sensor_id = 5` |
| **Columnar Full Scan** | 7.9M | 180.9 | `executeQueryColumnar` — no Arrow dependency |
| **Columnar Filtered** | 5.5M | 125.3 | Columnar stream with `WHERE` filter |
| Full Scan (eager) | 1.0M | 22.8 | `executeQuery` — all rows in memory |
| Full Scan (stream) | 836K | 19.1 | `executeQueryStream` — row-level iteration |
| Full Scan (chunked) | 637K | 14.6 | `nextChunk()` loop with per-row access |
| Aggregation | 195M | 4464.8 | `GROUP BY` — server-side, 10 result rows |

### Insert Performance

| Benchmark | Rows/sec | MB/s | Notes |
|---|---|---|---|
| **Insert (COPY)** | 1.4M | 31.2 | `Inserter.addRows()` batched in 50K chunks |

### Which Query API to Use

| API | Speed | Memory | Best for |
|---|---|---|---|
| `executeQueryToArrow` | Fastest | All in memory | Analytics, export, Arrow ecosystem |
| `executeQueryColumnar` | Fast | One chunk at a time | Streaming numeric processing |
| `executeQuery` | Moderate | All in memory | Small results, simple access |
| `executeQueryStream` | Moderate | One chunk at a time | Large results, row-level logic |

## Examples

The `examples/` directory contains runnable examples:

| Example | Description | Run |
|---|---|---|
| `complete-api-tour.mts` | Full 19-section tour in TypeScript | `npx tsx examples/complete-api-tour.mts` |
| `complete-api-tour.mjs` | Same tour in plain JavaScript | `node examples/complete-api-tour.mjs` |
| `typed-analytics.mts` | TypeScript analytics pipeline | `npx tsx examples/typed-analytics.mts` |
| `arrow-analytics.mjs` | Arrow integration deep-dive | `node examples/arrow-analytics.mjs` |
| `hyper-explorer/` | Web-based database inspector & generator | See below |

All examples require `HYPERD_PATH` to be set.

### Hyper Explorer

A full-stack web application (React + Express) for inspecting and generating `.hyper` files: schema browser, data preview, column analytics, SQL editor, database generator, and drag-and-drop file loading. See the **[Hyper Explorer README](examples/hyper-explorer/README.md)** for setup and full details.

## Documentation

| Document | Description |
|----------|-------------|
| [DEVELOPMENT.md](DEVELOPMENT.md) | Building from source, testing, publishing, contributing |
| [Node.js API Summary](../docs/NODEJS_API_SUMMARY.md) | One-page overview: architecture, query API tiers, Arrow integration |
| [Hyper Explorer README](examples/hyper-explorer/README.md) | Architecture and API for the web-based database inspector |

## License

Apache-2.0
