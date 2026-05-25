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

## Workspace model

Every session has TWO databases, plus optional user-attached ones:

- **Ephemeral primary** (default destination). Created fresh per
  session, deleted on exit. Unqualified SQL routes here. Use for
  scratch work and intermediate transformations the user doesn't
  need to keep.
- **Persistent attachment** under alias `\"persistent\"`. Survives
  across sessions at the platform-default path (or override via
  `--persistent-db`). Use when the user wants the data to stick
  around. Disabled when the server runs with `--ephemeral-only`.
- **User-attached writable databases** via `attach_database` with
  `writable: true`. Each lives in its own `.hyper` file under a
  user-chosen alias.

Pick a destination in one of two ways:

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
- `execute` — run DDL / DML (CREATE, INSERT, UPDATE, DELETE, DROP,
  ALTER, COPY). Disabled in read-only mode.
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
  query.
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
  `unwatch_directory`. `query`, `describe`, `sample`, `inspect_file`,
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
