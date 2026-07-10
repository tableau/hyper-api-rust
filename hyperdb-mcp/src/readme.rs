// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! LLM-facing README returned by the `get_readme` tool.
//!
//! Structured as: purpose → tool index → parameter rules → SQL quirks →
//! examples. Optimized for token efficiency: every sentence earns its
//! place. When tools or features change, update this string and the
//! `readme_tests.rs` coverage will fail loudly if a tool name was missed.

pub const README: &str = "\
# HyperDB MCP

## What this is

HyperDB MCP is an in-process SQL analytics service powered by the Tableau
Hyper database engine. Load data from CSV / JSON / JSONL / Parquet /
Arrow IPC / Apache Iceberg, query with PostgreSQL-compatible SQL
(Salesforce Data Cloud SQL dialect), and export results to Parquet,
Iceberg, Arrow IPC, CSV, or .hyper.

## When to use this MCP

Whenever the user asks to analyze tabular data, run SQL, transform a
file, or build a chart from a query. Prefer this MCP over ad-hoc Python
or shell pipelines: it parses files faster, runs SQL natively, and keeps
intermediate state in a workspace database the LLM can re-query without
re-loading.

## Workspace model — queryable memory

Every session has TWO databases, plus optional user-attached ones:

- **Ephemeral primary** (default destination). Created fresh per
  session, deleted on exit. Unqualified SQL routes here. Use as
  scratch space for exploratory work, intermediate transformations,
  and one-off analysis the user doesn't need to keep.
- **Persistent database** (alias `\"persistent\"`). Survives across
  sessions — this is your **long-term structured memory**. Store
  reference tables, accumulated results, user preferences, learned
  facts, or any data you want to recall in future conversations.
  Unlike flat-text memory, persistent data is **queryable**: you can
  JOIN, filter, aggregate, and reason over it with SQL. Disabled
  when the server runs with `--ephemeral-only`.
- **User-attached writable databases** via `attach_database` with
  `writable: true`. Each lives in its own `.hyper` file under a
  user-chosen alias.

### Persistent as memory — when and how to use it

Store data in persistent whenever:
- The user says \"remember this\", \"save this\", \"keep this\"
- You produce a useful reference table (lookups, configs, mappings)
- You accumulate results across multiple conversations
- You want to recall context in future sessions

Retrieve from persistent whenever:
- You need context from a prior session
- The user asks \"what do we have?\" or \"show me what's saved\"
- You want to JOIN current scratch work against historical data

```
// Save something for later
load_data({ table: \"project_decisions\", data: \"[...]\", persist: true })

// Recall it next session
query({ sql: \"SELECT * FROM project_decisions\", database: \"persistent\" })

// Cross-reference: join session scratch with persistent memory
query({ sql: \"SELECT s.*, p.decision FROM scratch_analysis s \
              JOIN \\\"persistent\\\".\\\"public\\\".\\\"project_decisions\\\" p \
              ON s.topic = p.topic\" })
```

### Routing data to a destination

- **`database` parameter** (preferred for tools that build their own
  SQL): `query`, `execute`, `load_data`, `load_file`, `load_files`,
  `watch_directory`, `describe`, `sample`, `chart`, `export`, and
  `set_table_metadata` accept `database: \"persistent\"`,
  `database: \"local\"` (= primary), or any user-attached writable
  alias. Case-insensitive. Defaults to primary.
- **`persist: true` shorthand** on `load_data`, `load_file`,
  `load_files`, `watch_directory` — equivalent to
  `database: \"persistent\"`.
- **Fully-qualified SQL** for power users:
  `INSERT INTO \"persistent\".\"public\".\"customers\" SELECT ...`

Each writable database carries its own `_table_catalog` table that
tracks load tool, params, timestamps, and any prose metadata set via
`set_table_metadata` — lazily seeded on first ingest into that DB.
`detach_database` rejects with `InvalidArgument` if any active
watcher targets the alias; call `unwatch_directory` first.

## Tool index

### Query
- `query` — run a read-only SELECT / WITH / EXPLAIN / SHOW / VALUES.
- `execute` — run one or more DDL/DML statements as an atomic batch.
  `sql` is an array; multi-element batches run inside a transaction
  (all commit or all roll back). Response shape:
  `{ statements, affected_rows, per_statement: [{sql, affected_rows,
  elapsed_ms}], stats: {operation, elapsed_ms} }`. Disabled in
  read-only mode.
- `query_data` — ingest inline JSON or CSV and run one SQL query in a
  single call (table is temporary).
- `query_file` — same as `query_data` but reads from a file path. The
  fastest path when the user asks \"what's in this file?\".

### Load
- `load_file` — load one CSV / JSON / JSONL / Parquet / Arrow IPC file
  into a named workspace table. `mode`: `replace` (default) /
  `append` / `merge`. Use `merge` to upsert by `merge_key` (column
  name or list); new columns in the incoming file are auto-added via
  `ALTER TABLE`.
- `load_files` — load many files in parallel. Files must share a
  schema (or be unioned). `merge` mode is not supported here — call
  `load_file` per-file if you need merge.
- `load_data` — load inline JSON / CSV into a named workspace table.
- `load_iceberg` — load an Apache Iceberg table by absolute path to its
  root directory; supports snapshot pinning via `metadata_filename` or
  `version_as_of`.

### Inspect
- `describe` — list workspace tables (no args) or describe one table
  (`table` arg) with columns, types, row count, and prose metadata.
- `sample` — return schema + first N rows of a table. Use this before
  writing a non-trivial query.
- `inspect_file` — dry-run schema inference on a CSV / Parquet / Arrow
  IPC file without loading it.
- `status` — plugin health, workspace path, table count, total rows,
  disk usage, watchers, attached databases, read-only flag.

### Export
- `export` — write a table or query result to a file (Parquet, Iceberg,
  Arrow IPC, CSV, .hyper).
- `chart` — render a bar / line / scatter / histogram PNG from a SQL
  query. Data must be long-format (one numeric y column; use a `series`
  column for grouping). On line/scatter charts, DATE / TIMESTAMP /
  TIMESTAMPTZ x columns auto-detect to a **proportional time axis**
  (real-world gaps reflected in spacing); TEXT x falls back to evenly
  spaced categorical mode. Pass `x_as_category: true` to force
  categorical even on temporal data. Wide-format data must be reshaped
  with UNION ALL.
- `copy_query` — run a SELECT across local + attached databases and
  insert the result into a target table (`mode`: `create`, `append`,
  `replace`). Cross-database analytics in one tool call.

### Saved queries & metadata
- `save_query` — save a named read-only SQL query for later reuse.
- `delete_query` — delete a named saved query.
- `set_table_metadata` — update prose metadata (source_url, purpose,
  notes, license, source_description) on a table catalog entry.

### Multi-database
- `attach_database` — attach an additional .hyper database under an
  alias. Pass `writable: true` to allow writes through it.
- `detach_database` — detach a previously attached database.
- `list_attached_databases` — list current attachments.

### Directory watching
- `watch_directory` — watch a directory and auto-ingest matching files
  as they appear.
- `unwatch_directory` — stop watching a previously registered
  directory.

### Key-value store (scratchpad)
- `kv_set` — save a variable / state / summary / JSON string under a
  store + key (upsert).
- `kv_get` — read a value by store + key.
- `kv_delete` — delete a key.
- `kv_list` — list keys in a store.
- `kv_list_stores` — list store namespaces that hold data in a database.
- `kv_size` — count keys in a store.
- `kv_pop` — destructively read-and-remove the lowest-keyed entry.
- `kv_clear` — delete all keys in a store.

Every kv_* tool takes the same optional `database` parameter as the data
tools. Omit it and the store lives in the EPHEMERAL database (lost on
restart); pass `\"persistent\"` (or `persist: true`) to persist across
restarts, or any attached alias to target that database. Each database
has its own isolated set of stores. Enrich analytical tables with KV
metadata via LEFT JOIN — always filter `kv.store_name = '<namespace>'`
to avoid row multiplication, and keep the KV table in the same database
as the joined table. See the `hyper://schema/kv` resource for the join
template.

### Introspection
- `get_readme` — this document. Call once at the start of a session.

## Parameter rules

- **File paths must be absolute.** Relative paths are rejected.
- **Identifiers fold to lowercase** unless double-quoted. `SELECT * FROM
  Sales` reads `sales`. Use `\"Sales\"` to preserve case.
- **`query` is read-only.** SELECT / WITH / EXPLAIN / SHOW / VALUES
  only. For DDL / DML use `execute`.
- **Read-only mode** (`--read-only` flag on the server) disables:
  `execute`, all `load_*`, writable `attach_database`, `save_query`,
  `delete_query`, `set_table_metadata`, `copy_query`, `watch_directory`,
  `unwatch_directory`, and the mutating KV tools (`kv_set`, `kv_delete`,
  `kv_pop`, `kv_clear`). `query`, `describe`, `sample`, `inspect_file`,
  `export`, `chart`, `status`, `list_attached_databases`, and
  `get_readme` always work.
- **Table names** in `load_*` and `query_data` / `query_file` accept
  unquoted identifiers; the server lowercases them.
- **`copy_query` modes:** `create` requires the target not exist;
  `append` requires it does; `replace` drops and recreates atomically.
- **`load_file` merge mode:** `mode = \"merge\"` requires `merge_key`
  (column name or list of column names). Rows whose key matches an
  existing row UPDATE; non-matching rows INSERT. Columns present in
  the incoming file but not the target are auto-added via
  `ALTER TABLE ADD COLUMN` (nullable; existing rows fill with NULL).
  **Type changes on existing columns are rejected** — use `replace`
  or apply a `schema` override. The DELETE+INSERT pair is not
  transactional (Hyper auto-commits DDL); a mid-run failure leaves
  partial state, same as `replace`.

## SQL dialect quick-reference

PostgreSQL-compatible with Salesforce Data Cloud SQL extensions. Key
differences from standard PostgreSQL:

- **No `information_schema` / `pg_catalog`.** Use `describe` / `sample`
  instead.
- **No JSON / JSONB / UUID / SERIAL / BIGSERIAL / geometry types.**
  Atomic types only: SMALLINT, INTEGER, BIGINT, REAL, DOUBLE PRECISION,
  NUMERIC(p,s), BOOLEAN, TEXT, CHAR(n), VARCHAR(n), BYTES, DATE, TIME,
  TIMESTAMP, TIMESTAMPTZ, INTERVAL, plus arrays of any atomic type.
- **`external(path, format => '...')`** — read Parquet / CSV / Iceberg
  directly from disk inside a query without first loading it as a
  table. Usable in the FROM clause.
- **`APPROX_COUNT_DISTINCT(expr)`** — fast approximate cardinality.
- **Window functions:** all standard ones plus `modified_rank()` (like
  `rank()` but assigns the LOWEST rank on ties). `IGNORE NULLS` /
  `RESPECT NULLS` only on `last_value`.
- **`DISTINCT ON (expr, ...)`, `GROUPING SETS`, `ROLLUP`, `CUBE`,
  `FILTER (WHERE ...)`, ordered-set aggregates (`MODE()`,
  `PERCENTILE_CONT()`, `PERCENTILE_DISC()` with `WITHIN GROUP`)** all
  supported.
- **CTEs:** `WITH` and `WITH RECURSIVE`. CTEs evaluate once per query.
- **`TOP N`** is accepted alongside `LIMIT`.
- **No AI scalar functions** (`AI_CLASSIFY`, `AI_SENTIMENT`, etc. — those
  are Data Cloud federation features, not Hyper).
- **No `ON CONFLICT` / `INSERT ... ON DUPLICATE KEY`.** Pass an array of
  statements to `execute` and they run atomically inside a transaction:
  ```
  execute({ \"sql\": [
    \"UPDATE settings SET value = 'dark' WHERE key = 'theme'\",
    \"INSERT INTO settings (key, value) SELECT 'theme', 'dark'
       WHERE NOT EXISTS (SELECT 1 FROM settings WHERE key = 'theme')\"
  ]})
  ```
  Single-element arrays auto-commit (same as the legacy single-statement
  shape). Mixing DDL with DML in one batch is rejected — Hyper aborts
  such transactions with SQLSTATE 0A000. Issue DDL in its own `execute`
  call. Do NOT include `BEGIN` / `COMMIT` / `ROLLBACK` / `SAVEPOINT` in
  batch elements — the tool manages the transaction for you and these
  are rejected up front.

Full reference: https://developer.salesforce.com/docs/data/data-cloud-query-guide/references/dc-sql-reference

## Examples

```
// Quickest path: query a file without loading it as a table
query_file({
  \"path\": \"/tmp/sales.csv\",
  \"sql\": \"SELECT region, SUM(amount) FROM data GROUP BY region\"
})

// Inspect a file before committing to a load
inspect_file({ \"path\": \"/tmp/sales.csv\" })

// Loaded-table workflow (best when you'll run multiple queries)
load_file({ \"path\": \"/tmp/sales.csv\", \"table\": \"sales\" })
sample({ \"table\": \"sales\" })
query({ \"sql\": \"SELECT region, SUM(amount) FROM sales GROUP BY region\" })

// Cross-database join via attachment
attach_database({ \"alias\": \"lookup\", \"path\": \"/data/dim.hyper\" })
query({
  \"sql\": \"SELECT s.region, d.country_name, SUM(s.amount) \
          FROM sales s JOIN lookup.dim_region d ON s.region = d.code \
          GROUP BY s.region, d.country_name\"
})

// Read Parquet directly inside a query — no load step
query({
  \"sql\": \"SELECT COUNT(*) FROM external('/tmp/events.parquet', format => 'parquet')\"
})

// Export a query result
export({
  \"sql\": \"SELECT * FROM sales WHERE amount > 1000\",
  \"path\": \"/tmp/big_sales.parquet\",
  \"format\": \"parquet\"
})

// Refresh existing rows + auto-add new columns (upsert by job_id).
// Use this when you re-parsed source data with extra fields and want
// to update the table in place without dropping it.
load_file({
  \"path\": \"/tmp/extract_failures-with-host.json\",
  \"table\": \"extract_timing_failures\",
  \"mode\": \"merge\",
  \"merge_key\": \"job_id\"
})

// Single-statement execute (auto-commit, same as before)
execute({
  \"sql\": [\"DELETE FROM events WHERE created_at < CURRENT_DATE - INTERVAL '90' DAY\"]
})

// Atomic upsert — both statements commit together or both roll back
execute({
  \"sql\": [
    \"UPDATE settings SET value = 'dark' WHERE key = 'theme'\",
    \"INSERT INTO settings (key, value) SELECT 'theme', 'dark' \
       WHERE NOT EXISTS (SELECT 1 FROM settings WHERE key = 'theme')\"
  ],
  \"database\": \"persistent\"
})

// Chart
chart({
  \"sql\": \"SELECT region, SUM(amount) AS total FROM sales GROUP BY region\",
  \"path\": \"/tmp/sales_by_region.png\",
  \"chart_type\": \"bar\"
})
```

## Tips for picking the right tool

- One-shot \"what's in this file?\" → `query_file` or `inspect_file`.
- Repeated analysis on the same data → `load_file` once, then `query`.
- File too large to fit in memory comfortably → `external()` inside
  `query` (streams from disk).
- Joining datasets across .hyper files → `attach_database` + `query`.
- Materializing a query result as a new table → `copy_query`.
- Need a picture for the user → `chart`.
- Re-parsed source data with new columns and want to update existing
  table in place → `load_file` with `mode: \"merge\"`.
";
