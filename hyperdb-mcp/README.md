# hyperdb-mcp

An MCP (Model Context Protocol) server that turns the Hyper columnar database into an instant SQL analytics engine. Data flows in from other MCP plugins or files, lands in Hyper automatically, and becomes queryable with SQL — no setup, no schema files, no database management.

Built on the pure-Rust [`hyperdb-api`](../hyperdb-api/) crate for maximum performance: 22M+ rows/sec inserts, 18M+ rows/sec queries, constant memory for billion-row results.

---

## Why

LLMs are powerful at reasoning but cannot natively crunch millions of rows. This plugin bridges that gap: another MCP tool produces data, the LLM passes it to `hyperdb-mcp`, Hyper ingests it and makes it SQL-queryable, the LLM runs analytical SQL, and results come back as JSON. Optionally export to CSV, Parquet, Apache Iceberg, Arrow IPC, or `.hyper` (opens directly in **Tableau Desktop**).

---

## Features

- **Zero setup** — `HyperProcess` auto-starts the Hyper server
- **Any data in** — JSON, CSV, Parquet, Arrow IPC, Apache Iceberg; schema inferred or exact
- **SQL at scale** — thousands to billions of rows
- **Data out** — export to CSV, Parquet, Apache Iceberg, Arrow IPC, or `.hyper` (Tableau Desktop-ready)
- **One-shot queries** — `query_file("/tmp/sales.csv", "SELECT ...")` — single call, zero management
- **Persistent workspace** — load multiple tables, JOIN across them, persist across sessions
- **Read-only safe mode** — `--read-only` flag for safe deployment
- **Schema resources** — auto-discover table schemas via `resources/list`
- **Guided prompts** — `analyze-table`, `compare-tables`, `data-quality`, `suggest-queries`
- **Inline charts** — bar/line/scatter/histogram as PNG or SVG
- **Incremental ingest** — `watch_directory` monitors for `.ready` sentinel files
- **Performance telemetry** — every response includes throughput stats
- **Smart schema inference** — exact (Arrow/Parquet), structural (JSON), heuristic (CSV) with full-file numeric widening
- **Pre-ingest file inspection** — `inspect_file` dry-runs the same inference without touching Hyper so LLMs can build safe schema overrides in one shot
- **Partial schema overrides** — supply just the columns you want to correct (e.g. `{"population":"BIGINT"}`) — the rest keep their inferred type
- **Rich resource surface** — workspace readme, per-table JSON and CSV samples, and one JSON + one CSV resource per table so LLMs can orient themselves via `resources/list` without any tool calls
- **Saved queries** — register named read-only SQL with `save_query`; each query becomes `hyper://queries/{name}/definition` (metadata) + `hyper://queries/{name}/result` (live re-run). Persisted in `--workspace` mode, session-only otherwise
- **Live resource-update notifications** — MCP clients can `resources/subscribe` to any `hyper://...` URI; the server fires `notifications/resources/updated` after every ingest, DDL, watcher event, or saved-query mutation

---

## Installation

### From npm

> **Requirement:** Node.js **v21 or later**. Earlier versions ship an
> older `npx` whose argument parsing is incompatible with the
> `npx -y hyperdb-mcp` invocation in the MCP config below. If you're
> on an older Node, see [Upgrading Node.js with nvm](#upgrading-nodejs-with-nvm)
> below.

```bash
npm install -g hyperdb-mcp
```

The npm package bundles both the `hyperdb-mcp` binary and the `hyperd` database server — no additional setup required.

### Upgrading Node.js with nvm

`nvm` (Node Version Manager) makes it easy to install and switch between Node.js versions.

**macOS / Linux** ([nvm-sh/nvm](https://github.com/nvm-sh/nvm)):
```bash
# install nvm if you don't have it
curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash

# install and use the latest LTS (>= 21)
nvm install --lts
nvm use --lts
node --version    # should report v22.x.x or newer
```

**Windows** ([coreybutler/nvm-windows](https://github.com/coreybutler/nvm-windows)): download the installer, then in a new shell:
```powershell
nvm install lts
nvm use lts
node --version
```

After upgrading, restart your MCP client so it picks up the new Node binary on `PATH`.

### Building from Source

```bash
cd hyper-api-rust
cargo build --release -p hyperdb-mcp
```

The binary is at `target/release/hyperdb-mcp`. When building from source, the `hyperd` executable must be available separately — set the `HYPERD_PATH` environment variable or ensure it's on your `PATH`.

### MCP Client Configuration

Each AI tool reads MCP server config from a different file but uses the same JSON shape. The base config block using npx (recommended):
```json
{
  "mcpServers": {
    "HyperDB": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "hyperdb-mcp"]
    }
  }
}
```

Or if you built from source:
```json
{
  "mcpServers": {
    "HyperDB": {
      "type": "stdio",
      "command": "/path/to/hyperdb-mcp",
      "env": {
        "HYPERD_PATH": "/path/to/hyperd"
      }
    }
  }
}
```

For a **persistent workspace** (tables survive across sessions), add `"args"`:
```json
"args": ["--workspace", "/path/to/my-project.hyper"]
```
This is still **experimental** and will only work with only one session at a time since the Hyper database is locked by Hyper. Each session is isolated and has its own Hyper instance running. Future work will allow multiple sessions to share the same database but requires work to spin up a shared Hyper instance.

#### Claude Code / AI Suite

Create or edit `~/.claude/.mcp.json` (global) or `.mcp.json` in the project root (project-scoped). Use the base config block above.

After adding the config:
1. Start a new Claude Code session. You'll be prompted to approve the server on first use.
2. **Auto-approve tools (optional):** Add `"mcp__HyperDB__*"` to the `permissions.allow` array in `~/.claude/settings.json`.

#### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows). Use the base config block above.

#### Cursor

Edit `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (project root). Use the base config block above.

#### Other MCP Clients

Any tool that supports the MCP stdio transport can use this server. Point it at the `hyperdb-mcp` binary and set `HYPERD_PATH` in the environment.

---

## MCP Tools

### One-Shot Tools

#### `query_data`

Ingest inline data and run a SQL query in a single call.

```
query_data(data: '[{"region":"West","revenue":1200},...]', sql: 'SELECT region, SUM(revenue) FROM data GROUP BY region')
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `data` | string | yes | JSON array of objects, or CSV text |
| `sql` | string | yes | SQL query to run against the data |
| `format` | string | no | `"json"` or `"csv"` — auto-detected if omitted |
| `table_name` | string | no | Table name for use in SQL — defaults to `"data"` |
| `schema` | object | no | Partial column-name → type map (see [Schema Overrides](#schema-overrides)) |

#### `query_file`

Ingest a file and run a SQL query in a single call. Streams from disk — handles files of any size.

```
query_file(path: '/tmp/sales.parquet', sql: 'SELECT TOP 10 * FROM sales ORDER BY amount DESC')
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Path to CSV / JSON / JSONL / Parquet / Arrow IPC file |
| `sql` | string | yes | SQL query to run |
| `table_name` | string | no | Table name — defaults to filename stem |
| `schema` | object | no | Partial column-name → type map (see [Schema Overrides](#schema-overrides)) |

### Workspace Tools

#### `load_data`

Load inline data into a named workspace table.

```
load_data(table: 'customers', data: '[{"id":1,"name":"Alice"},...]')
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `table` | string | yes | Table name |
| `data` | string | yes | JSON array of objects, or CSV text |
| `format` | string | no | `"json"` or `"csv"` — auto-detected |
| `mode` | string | no | `"replace"` (default) or `"append"` |
| `schema` | object | no | Partial column-name → type map (see [Schema Overrides](#schema-overrides)) |

#### `load_file`

Load a file into a named workspace table.

```
load_file(table: 'orders', path: '/tmp/orders.csv')
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `table` | string | yes | Table name |
| `path` | string | yes | Path to CSV / JSON / JSONL / Parquet / Arrow IPC file |
| `mode` | string | no | `"replace"` (default) or `"append"` |
| `schema` | object | no | Partial column-name → type map (see [Schema Overrides](#schema-overrides)) |

When you're unsure of the right types — or recovering from a previous
`SCHEMA_MISMATCH` — call [`inspect_file`](#inspect-file) first. It reports the
exact schema `load_file` would use plus per-column `min` / `max` / `null_count`
so you can build a minimal, correct override in one shot.

#### `load_iceberg`

Load an [Apache Iceberg](https://iceberg.apache.org/) table into a named
workspace table. Pass the absolute path to the Iceberg table root (the
directory containing `metadata/` and `data/`); hyperd's native Iceberg
reader derives the schema and resolves the snapshot.

```
load_iceberg(table: 'sales', path: '/lake/warehouse/db/sales')
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `table` | string | yes | Target Hyper table name |
| `path` | string | yes | Absolute path to the Iceberg table root directory |
| `mode` | string | no | `"replace"` (default) or `"append"` |
| `metadata_filename` | string | no | Pin a specific snapshot, e.g. `"v2.metadata.json"`. Omit for latest. |
| `version_as_of` | integer | no | Pin a snapshot by version number |

Schema overrides are not accepted — hyperd derives the schema from the
Iceberg table metadata.

#### `query`

Run a **read-only** SQL query against the workspace. Accepts `SELECT`, `WITH`, `EXPLAIN`, `SHOW`, `VALUES`. For DDL/DML use `execute`.

```
query(sql: 'SELECT c.name, SUM(o.amount) FROM orders o JOIN customers c ON o.customer_id = c.id GROUP BY c.name')
```

#### `execute`

Execute a **mutating** SQL statement: `CREATE TABLE`, `INSERT`, `UPDATE`, `DELETE`, `DROP TABLE`, `ALTER`, `COPY`, etc. Returns the affected row count. Disabled in read-only mode.

```
execute(sql: 'CREATE TABLE archived_orders AS SELECT * FROM orders WHERE year < 2024')
```

#### `describe`

List all workspace tables with their schemas, column types, and row counts.

#### `sample`

Return the schema, total row count, and first N rows of a table in a single call.

```
sample(table: 'orders', n: 10)
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `table` | string | yes | Table name |
| `n` | int | no | Rows to return (default: 5, clamped to 1..=100) |

### Diagnostics

#### `inspect_file`

Dry-run schema inference on a CSV, Parquet, or Arrow IPC file **without ingesting
it**. Returns the exact schema `load_file` / `query_file` would use (including
the full-file numeric widening pass) plus per-column `min`, `max`, `null_count`,
and `sample_values`. Nothing is written to Hyper and `hyperd` is not even
started.

Use it **before** `load_file` whenever you are unsure about types, or **after** a
`SCHEMA_MISMATCH` failure to pick the right widening. The LLM can feed the
reported `type` + `min` / `max` directly into a partial `schema` override on the
subsequent `load_file` call.

```
inspect_file(path: '/tmp/owid-population.csv')
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Path to CSV / JSON / JSONL / Parquet / Arrow IPC file |
| `sample_rows` | int | no | Sample values / rows per column (default 5, clamped 1..=50) |

Response shape:

```json
{
  "file_format": "csv",
  "row_count": 63000,
  "file_size_bytes": 4831204,
  "columns": [
    { "name": "Entity",     "type": "TEXT",   "nullable": true, "null_count": 0,   "sample_values": ["Afghanistan", ...] },
    { "name": "Year",       "type": "INT",    "nullable": true, "null_count": 0,   "min": 1800, "max": 2023, "sample_values": ["1800", ...] },
    { "name": "Population", "type": "BIGINT", "nullable": true, "null_count": 12,  "min": 500, "max": 8002572256, "sample_values": ["4000000", ...] }
  ],
  "sample_rows": [ { "Entity": "Afghanistan", "Year": "1800", "Population": "2805829" } ]
}
```

`sample_values` and `sample_rows` are **always strings**, regardless of the inferred column `type` — they report what the file contains on disk, before any type coercion, so the LLM can compare the raw text against `min` / `max` when building a `schema` override. Use `type` (and `min` / `max`) for the typed view; use `sample_values` for the raw view.

### Saved Queries

Register a named read-only SQL query once; read its live result as many
times as you like via a resource URI. Useful for dashboard-style recurring
views and for giving LLMs a stable "bookmark" set of key queries that
resources/list advertises up front.

Each saved query produces **two** resources:

- `hyper://queries/{name}/definition` — the stored SQL plus metadata
  (description, `created_at`) as JSON.
- `hyper://queries/{name}/result` — re-runs the SQL on every read and
  returns `{ name, result: [...], stats: {...} }`.

**Persistence:** queries saved while `--workspace <path>` is set are
stored in the `_hyperdb_saved_queries` meta-table inside the `.hyper`
file and survive server restarts. In ephemeral workspaces they live only
for the lifetime of the server process.

#### `save_query`

```
save_query(name: 'top_5_customers', sql: 'SELECT customer, SUM(amount) AS total FROM orders GROUP BY customer ORDER BY total DESC LIMIT 5', description: 'Biggest spenders this year')
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Unique identifier used as the URI path component |
| `sql` | string | yes | Read-only SQL (SELECT / WITH / EXPLAIN / SHOW / VALUES) |
| `description` | string | no | Human-friendly summary |

Duplicate names are rejected with `INVALID_ARGUMENT` — use `delete_query`
first if you intend to overwrite. Non-read-only SQL is rejected with
`SQL_ERROR`. Disabled in read-only mode.

#### `delete_query`

```
delete_query(name: 'top_5_customers')
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Name of the saved query to remove |

Returns `{ "deleted": true }` when the query existed, `{ "deleted": false }`
when it did not (no error on unknown names). Disabled in read-only mode.

### Export Tools

#### `export`

Write query results or a table to a file.

```
export(table: 'orders', path: '~/Desktop/orders.parquet', format: 'parquet')
export(sql: 'SELECT ...', path: '~/Desktop/analysis.hyper', format: 'hyper')
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `sql` | string | no | Query to export (if omitted, exports whole table) |
| `table` | string | no | Table name (used if `sql` omitted) |
| `path` | string | yes | Output file path |
| `format` | string | yes | `"csv"`, `"parquet"`, `"iceberg"`, `"arrow_ipc"`, or `"hyper"` |

The `"hyper"` format produces a `.hyper` file that opens directly in **Tableau Desktop**.

### Visualization

#### `chart`

Render a chart from a SQL query and return it inline as an image.

```
chart(sql: 'SELECT product, SUM(revenue) as total FROM sales GROUP BY product', chart_type: 'bar', x: 'product', y: 'total', title: 'Revenue by Product')
```

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `sql` | string | yes | Read-only SQL query returning the data to plot |
| `chart_type` | string | yes | `bar`, `line`, `scatter`, or `histogram` |
| `x` | string | yes* | X-axis column (for histogram, the value column) |
| `y` | string | yes* | Y-axis column (not required for histogram) |
| `series` | string | no | Grouping column for multi-series plots |
| `title` | string | no | Chart title |
| `format` | string | no | `png` (default) or `svg` |
| `width` | int | no | Pixels (default 800, clamped 200..4096) |
| `height` | int | no | Pixels (default 480, clamped 150..4096) |
| `bins` | int | no | Histogram bins (default 20, clamped 1..500) |

Returns an `ImageContent` (base64 PNG or SVG) plus a stats JSON block.

### Incremental Ingest

#### `watch_directory` / `unwatch_directory`

Monitor a directory for data files and auto-append them to a target table.

```
watch_directory(path: '/tmp/inbox', table: 'events')
unwatch_directory(path: '/tmp/inbox')
```

**Producer protocol (`.ready` sentinel):**

1. Write data file (e.g. `foo.csv`) and close it.
2. Create a zero-byte companion `foo.csv.ready` — this is the atomic signal.
3. Poll for the absence of `foo.csv.ready` to confirm the watcher is done.

On success, both files are deleted. On failure, both are moved to `failed/` with a `.error` JSON file.

Key properties:
- **One directory, one table, append mode** — files must match the target schema.
- **Initial sweep** — pre-existing `.ready` files are processed immediately.
- **Read-only mode** — `watch_directory` is blocked; `unwatch_directory` is always allowed.
- **Cleanup** — dropping the server or calling `unwatch_directory` terminates the background thread.

### Utility Tools

#### `status`

Returns plugin health, workspace mode, table count, total rows, disk usage, read-only flag, and active directory watchers with per-watcher stats.

---

## MCP Resources

The server exposes workspace state as MCP **Resources**, discoverable via
`resources/list`. Each resource advertises its own MIME type so clients
can route it appropriately (LLM context vs. file download vs. chart).

| URI | MIME | Content |
|-----|------|---------|
| `hyper://workspace` | `application/json` | Workspace mode, table count, total rows, disk usage |
| `hyper://tables` | `application/json` | Full list of tables with schemas and row counts |
| `hyper://readme` | `text/markdown` | Workspace overview as markdown: table catalog, related resources per table, and tool hints for a cold-started LLM |
| `hyper://tables/{name}/schema` | `application/json` | Columns, types, nullability, and row count for one table |
| `hyper://tables/{name}/sample` | `application/json` | First 5 rows of a table as JSON, with schema |
| `hyper://tables/{name}/csv-sample` | `text/csv` | First 20 rows of a table as CSV, header-first |
| `hyper://queries/{name}/definition` | `application/json` | Stored SQL + metadata for a saved query |
| `hyper://queries/{name}/result` | `application/json` | Live result of a saved query — re-runs on every read |

Resource templates (discoverable via `resources/templates/list`):

- `hyper://tables/{name}/schema`
- `hyper://tables/{name}/sample`
- `hyper://tables/{name}/csv-sample`
- `hyper://queries/{name}/definition`
- `hyper://queries/{name}/result`

The internal `_hyperdb_saved_queries` meta-table used to persist saved
queries is deliberately hidden from `resources/list` and
`hyper://tables` — callers see only user-visible data tables.

### Resource-update notifications

HyperDB advertises both the `resources.subscribe` and
`resources.listChanged` capabilities in its `initialize` response. Clients
can subscribe to any `hyper://...` URI via `resources/subscribe` and will
then receive `notifications/resources/updated` messages whenever the
server detects a change, without polling.

The server fires **targeted** updates for the URIs affected by each kind
of mutation:

| Trigger | Updated URIs | `resources/list_changed`? |
|---|---|---|
| `load_data` / `load_file` (replace mode) | `hyper://workspace`, `hyper://tables`, `hyper://readme`, per-table schema + sample + csv-sample | Yes |
| `load_data` / `load_file` (append mode) | Same per-table + summary URIs | No &sup1; |
| `watch_directory` ingest of a `.ready` pair | Same per-table + summary URIs | No &sup1; |
| `execute` (INSERT / UPDATE / DELETE) | Workspace summary URIs | No |
| `execute` (CREATE / DROP / ALTER / TRUNCATE / RENAME) | Workspace summary URIs | Yes |
| `save_query` | (none per-URI) | Yes — two new `hyper://queries/{name}/...` resources |
| `delete_query` | `hyper://queries/{name}/definition`, `hyper://queries/{name}/result` | Yes — two resources disappeared |

&sup1; Append-mode ingest (both `load_*` and the watcher) auto-creates the target table when it doesn't exist, but **does not** fire `list_changed` for that creation. Clients that need to discover watcher-created tables should re-read `hyper://tables` after subscribing, or use the per-table `updated` notification as a trigger to refresh their list. Tracked in `DEVELOPMENT.md` as tech debt.

Notifications are fire-and-forget — send failures (typically due to a
client disconnect) are logged at the `debug` level and the registry
prunes dead peers lazily. This keeps mutation paths fast and free of
back-pressure concerns.

All JSON-typed resources return a pretty-printed object; Markdown and
CSV resources are returned verbatim.

---

## MCP Prompts

Four guided analytical workflows registered as MCP **Prompts**.

| Prompt | Arguments | What it does |
|--------|-----------|--------------|
| `analyze-table` | `table` | Schema walkthrough, column statistics, data quality flags |
| `compare-tables` | `table_a`, `table_b` | Schema alignment, JOIN key suggestions, analytical opportunities |
| `data-quality` | `table` | Systematic NULL / duplicate / cardinality / outlier checks |
| `suggest-queries` | `table`, `goal?` | 5 analytical SQL queries with explanations, optionally goal-guided |

---

## Read-Only Mode

```bash
hyperdb-mcp --workspace ~/analytics.hyper --read-only
```

- **Allowed:** `query`, `query_data`, `query_file`, `describe`, `sample`, `inspect_file`, `status`, `export`
- **Blocked:** `execute`, `load_data`, `load_file`, `watch_directory`, `save_query`, `delete_query` — return `READ_ONLY_VIOLATION`
- **Resources, prompts, and resource subscriptions** work normally — read-only clients can still subscribe to `hyper://...` URIs and receive notifications when other (non-read-only) connections mutate state

The `query` tool also enforces read-only at the SQL level — only `SELECT`/`WITH`/`EXPLAIN`/`SHOW`/`VALUES` are accepted.

---

## Data Flow Patterns

- **Small data (LLM relay):** For <10K rows. The LLM gets data from another plugin and passes it inline via `query_data`.
- **Large data (file intermediary):** For thousands to billions of rows. Source plugin exports to a file, the LLM calls `query_file`. Data never enters the LLM context — constant memory regardless of file size.

---

## Schema Inference

Three tiers, chosen automatically based on the data source:

| Tier | Source | How |
|------|--------|-----|
| **Exact** | Arrow IPC, Parquet | Schema read from file metadata. Types preserved exactly. |
| **Structural** | JSON | All objects scanned. Per-column type widening: Int → BigInt → Double. Mixed types → TEXT. |
| **Heuristic** | CSV | Header row for names, first 1,000 rows sampled for types. A second full-file streaming pass then **widens** numeric columns if needed (INT → BIGINT → NUMERIC(38,0); INT/BIGINT → DOUBLE PRECISION if any later row contains a decimal). |

**JSON file shapes.** `load_file` and `query_file` accept two JSON
representations and auto-detect between them from the first non-whitespace
byte: a top-level JSON array of objects (e.g. `[{...}, {...}]`) or
newline-delimited JSON (JSONL / NDJSON — one JSON object per line, the
format hyperd's own logs use). Blank lines are tolerated. Malformed
JSONL surfaces a `SCHEMA_MISMATCH` error naming the offending line
number.

**Content sniffing for unknown extensions.** Files with extensions the
dispatcher doesn't recognize (`.log`, `.txt`, no extension at all) are
classified by peeking at the first non-whitespace byte: `[` or `{`
routes to JSON, anything else to CSV. This means hyperd's raw `.log`
files load through `load_file` directly, no rename or preprocessing
required. Binary formats (`.parquet`, `.arrow`, `.ipc`, `.feather`,
`.pq`) always win by extension since they're not text-sniffable.
`inspect_file` uses the exact same dispatcher so its report can never
disagree with what `load_file` would do.

**CSV NULL handling.** Unquoted empty cells (`,,`) load as SQL NULL —
matching PostgreSQL's CSV convention and `inspect_file`'s `null_count`
diagnostics. Quoted empty strings (`,"",`) load as the literal empty
string. This means downstream `WHERE col IS NULL` works directly without
a defensive `OR col = ''` clause.

The full-file CSV widening pass specifically protects against the "big value
hidden at the end of the file" failure mode — e.g. an aggregate row whose
`population` is ~8 billion tucked in after 60 000 country-sized rows. Without
it, the first-pass sample would pick `INT` and the COPY would fail with
`SCHEMA_MISMATCH` / SQLSTATE 22003 mid-ingest.

For implementation details (widening rules, type mapping tables), see the
module docs in `src/schema.rs` and `src/ingest_arrow.rs`.

### Schema Overrides

Every data-in tool (`query_data`, `query_file`, `load_data`, `load_file`)
accepts an optional `schema` parameter: a **partial** map from column name to
Hyper SQL type.

```json
{ "schema": { "population": "BIGINT", "order_date": "DATE" } }
```

Semantics:

- Keys are matched to columns **by name** (case-sensitive). Column order in
  the JSON object does not need to match the file — the inferred order from
  the file is preserved.
- Columns **not** listed in the override keep their inferred type. You only
  specify the columns you want to correct.
- Unknown column names and unknown type strings are rejected up front with a
  `SCHEMA_MISMATCH` error that lists the real column names, so the LLM can
  self-correct without another round-trip.
- Supported type strings: `INT`, `BIGINT`, `NUMERIC(p,s)` (e.g.
  `NUMERIC(38,0)` or `NUMERIC(12,2)`), `DOUBLE PRECISION`, `TEXT`, `BOOL`,
  `DATE`, `TIMESTAMP`.

**Recommended workflow for unfamiliar data:**

1. Call `inspect_file` → read the reported `type` + `min` / `max` per column.
2. For any column whose `max` exceeds its inferred type's range, or where
   you want stricter parsing than CSV heuristics give, build a partial
   override.
3. Pass it to `load_file` / `query_file`.

---

## SQL Dialect

Hyper uses the Salesforce Data Cloud SQL dialect (PostgreSQL-compatible with extensions). Supports `SELECT`, JOINs, subqueries, CTEs, window functions, aggregations, DDL, DML, and `COPY FROM`.

Full reference: [Data Cloud SQL Reference](https://developer.salesforce.com/docs/data/data-cloud-query-guide/references/dc-sql-reference/data-cloud-sql-context.html)

---

## CLI Reference

```
hyperdb-mcp [OPTIONS]

Options:
  --workspace <PATH>    Path to the `.hyper` workspace file for persistent mode (omit for ephemeral)
  --read-only           Disable mutating tools (execute, load_data, load_file, save_query, delete_query, watch_directory)
  --bare                Skip MCP-managed auxiliary tables (`_table_catalog`) and force saved queries into in-memory storage, even with --workspace

Environment:
  HYPERD_PATH           Path to hyperd binary (auto-detected if on PATH)
```

---

## Error Handling

Errors include a machine-readable code and a suggestion:

| Code | When | Recovery |
|---|---|---|
| `HYPERD_NOT_FOUND` | `hyperd` not found | Set `HYPERD_PATH` or install Hyper |
| `FILE_NOT_FOUND` | File path doesn't exist | Verify the path |
| `UNSUPPORTED_FORMAT` | Unrecognized file type | Specify `format` explicitly |
| `SCHEMA_MISMATCH` | Data doesn't match inferred types, numeric overflow (SQLSTATE 22003), or invalid text for target type (SQLSTATE 22P02) | Call `inspect_file` then retry with a partial `schema` override (e.g. `{"population":"BIGINT"}` or `{"id":"TEXT"}`) |
| `SQL_ERROR` | Invalid SQL | Fix the query |
| `TABLE_NOT_FOUND` | Table doesn't exist | Use `describe` to list tables |
| `READ_ONLY_VIOLATION` | Mutating op in read-only mode | Use `query_*` / `inspect_file`, or restart without `--read-only` |
| `CONNECTION_LOST` | `hyperd` crashed or wire protocol desynchronized | Retry — the server tears down the engine and reconnects on the next call |

Server-returned errors include a machine-readable `code`, a `message`, and a
`suggestion` with concrete retry guidance. The `SCHEMA_MISMATCH` suggestion for
an overflow names the workflow directly: "call `inspect_file`, then retry with
a partial schema override", so the LLM does not need to infer the recovery
steps from the SQLSTATE alone.

---

## Troubleshooting

**Tools not discovered by the client** — Verify the `initialize` response advertises `"capabilities": {"tools": {}}`. Pipe a raw `initialize` JSON-RPC request to the binary to check.

**Server registered but tools not callable (Claude Code)** — Add `"mcp__HyperDB__*"` to the `permissions.allow` array in `~/.claude/settings.json`.

**hyperd not found** — Set `HYPERD_PATH` in the MCP server's `env` config, or place `hyperd` on your `PATH`.

---

## Related Documentation

- **[Main README](../README.md)** — Getting started with the Hyper API
- **[hyperdb-api](../hyperdb-api/)** — Core Rust API (sync/async connections, inserter, query)
- **[DEVELOPMENT.md](DEVELOPMENT.md)** — Internal architecture, design decisions, contributor guide
- **[ROADMAP.md](ROADMAP.md)** — Forward-looking design sketches for features that aren't built yet
- **[Design Spec](../docs/specs/hyperdb-mcp-design.md)** — Full design document
