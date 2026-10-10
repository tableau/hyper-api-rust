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
intermediate state in a local database the LLM can re-query without
re-loading.

## Database model — queryable memory

Every session has TWO databases, plus optional user-attached ones:

- **Local database** (ephemeral and the default destination). Created
  fresh per session, deleted on exit, and addressed as `\"local\"`.
  Unqualified SQL routes here; use it for scratch data.
- **Persistent database** (alias `\"persistent\"`). Survives across
  sessions as queryable long-term structured memory. Disabled with
  `--ephemeral-only`.
- **Attached databases** via `attach_database`. Each lives in its own
  `.hyper` file under a user-chosen canonical lowercase alias. Attachments
  are read-only by default; pass `writable: true` when writes are needed.

### Persistent as memory

Use persistent for reference data, preferences, accumulated results, or
anything the user asks to remember or query in a future session.

```
load_data({ table: \"project_decisions\", data: \"[...]\", persist: true })
query({ sql: \"SELECT * FROM project_decisions\", database: \"persistent\" })
```

### KV store vs. a custom table — which to remember with

When you need to remember something, pick the lighter tool:

- **A few scraps** — variables, flags, summaries, JSON, queue entries —
  use the key-value store (`kv_set` / `kv_get`); no schema or DDL.
- **Structured rows** you'll filter, JOIN, or aggregate — use a real
  table (`load_data` / `execute CREATE TABLE`), so SQL can reason over
  typed columns.

Both persist the same way: default is the local database (lost on
restart); pass `database: \"persistent\"` to keep either one across
sessions. The KV tools, `load_data`, `load_file`, `load_files`, and
`watch_directory` also accept the `persist: true` shorthand; `execute`
takes `database` only, and `load_iceberg` always loads into local.

### Routing data to a destination

- **`database` parameter** (preferred for tools that build their own
  SQL): `query`, `execute`, `load_data`, `load_file`, `load_files`,
  `watch_directory`, `describe`, `sample`, `chart`, `export`, and
  `set_table_metadata` accept `database: \"persistent\"`,
  `database: \"local\"`, or any user-attached alias (write tools require
  a writable attachment). Case-insensitive. Defaults to local.
- **`persist: true` shorthand** on `load_data`, `load_file`,
  `load_files`, `watch_directory` — equivalent to
  `database: \"persistent\"`.
- **Fully-qualified SQL** for power users:
  `INSERT INTO \"persistent\".\"public\".\"customers\" SELECT ...`

### Chart delivery and presentation

Long and Unicode labels are not truncated or auto-sized; if they clip,
increase `width` or `height`.
Log bars start at the positive lower bound, not zero.

`chart` is a bounded quick diagnostic, not a dashboard system. With no
`output_path`, its PNG (default) or SVG is returned `inline` and no file
is written. Supplying `output_path` writes the file and still returns it
inline; set `inline=false` for disk-only output (an omitted path then gets
an auto-generated temp file). Set `overwrite=false` to refuse an existing
destination. The path extension and explicit `format`, when both supplied,
must agree. Use `database` to route the SQL to local, persistent, or an
attached database.

Bars are vertical by default; set `bar_orientation` to `horizontal` for
rankings. `label_values=true` writes each original y scalar beside its
bar. `show_legend` defaults to true; set it false to suppress the legend.
With a `series` column, `color_map` maps series names to hex colors;
`label_points=true` labels line/scatter points and suppresses their legend.
`y_scale` defaults to `linear`; `log`
applies to the data-role y measure, including the physical x axis of
horizontal bars. Log values and ranges must be finite and strictly
positive (> 0): zero and negative values are invalid, logarithmic
histograms are unsupported, and an explicit range must contain every
plotted value. Explicit `x_range` and `y_range` bounds must also be finite,
strictly increasing, and representable.

`x_range` means something different per chart type. On line/scatter it sets
the axis bounds only: out-of-range points are still counted in
`rows_plotted` but render invisibly (clipped), never moved. On a histogram
it is the binning extent, and values outside it are **excluded from the
bins** rather than folded into the edge bins, so `rows_plotted` counts only
the values actually binned and `excluded_out_of_range` appears in the stats
block reporting how many were dropped. A histogram `x_range` containing no
data renders empty at the requested extent instead of erroring. Bar charts
ignore `x_range` entirely (their x positions are categorical). `bins` is
clamped to 1..=500.

Line/scatter DATE, TIMESTAMP, and TIMESTAMPTZ x values use a proportional
time axis automatically. TEXT is categorical; set `x_as_category=true`
to deliberately give temporal observations even spacing. Bars always
treat x as categorical.

Every successful database-routed tool response carries canonical
`resolved_database`: `\"local\"`, `\"persistent\"`, or the lowercase attached
alias. `copy_query` also retains `target_database`.

## Tool index

### Query
- `query` — run a read-only SELECT / WITH / EXPLAIN / SHOW / VALUES.
  Max 10,000 rows; check `truncated` / `total_rows`.
- `execute` — run one or more DDL/DML statements as an atomic batch.
  `sql` is an array; multi-element batches run inside a transaction
  (all commit or all roll back). Disabled in read-only mode.
- `query_data` — ingest inline JSON or CSV and run one SQL query in a
  single call (table is temporary).
- `query_file` — same as `query_data` but reads from a file path. The
  fastest path when the user asks \"what's in this file?\".

### Load

**Format preference (ingest and export):** Parquet (fastest, server-side,
preserves every type incl. NUMERIC precision / DATE / TIMESTAMP — best for
large data) > Arrow IPC (very fast, no compression; schema overrides
rejected since its schema is authoritative) > CSV (portable but types are
inferred on load / lost on export) > JSON / JSONL (small or irregular data
only — parsed row-by-row). Iceberg is a directory of Parquet for data-lake
interop; `.hyper` export snapshots every table for Tableau.

- `load_file` — load one CSV / JSON / JSONL / Parquet / Arrow IPC file
  into a named database table. `mode`: `replace` (default) /
  `append` / `merge`. Use `merge` to upsert by `merge_key` (column
  name or list); new columns in the incoming file are auto-added via
  `ALTER TABLE`.
- `load_files` — load many files in parallel; each entry
  `{path, table, mode?}` loads into its own table. `merge` mode is
  not supported here — call `load_file` per-file if you need merge.
- `load_data` — load inline JSON / CSV into a named database table.
- `load_iceberg` — load an Apache Iceberg table by absolute path to its
  root directory; supports snapshot pinning via `metadata_filename` or
  `version_as_of`.

### Inspect
- `describe` — list local tables (no args) or describe one table
  (`table` arg) with columns, types, row count, and prose metadata.
  **Defaults to the local database** — pass `database: \"persistent\"`
  (or an attached alias) to list/inspect durable tables. `status` reports
  table *counts* only, never names, so check the right database here
  before assuming a persistent table is missing.
- `sample` — return schema + first N rows of a table. Use this before
  writing a non-trivial query.
- `inspect_file` — dry-run schema inference on a CSV / Parquet / Arrow
  IPC / JSON file without loading it.
- `status` — plugin and native/API identity; daemon/Hyper connection facts;
  local/persistent paths; table count; disk usage; watchers; attachments;
  read-only flag.
  `engine.connection` gives the `hyperd` endpoint in the forms another
  Hyper client needs: `transport` (`tcp` / `unix_domain_socket` /
  `named_pipe`), `host` + `port` (TCP only, else null), `socket_path`
  (IPC only, else null), and `connection_descriptor` — the scheme-
  qualified string `hyperd` emits and the Hyper API accepts, e.g.
  `tab.tcp://host:port`. The shared daemon uses IPC; a private
  `hyperd` (`--no-daemon` or daemon fallback) uses TCP.
  Both full and degraded responses report `default_database: \"local\"`.
  When
  `engine_busy: true`, the response is partial and non-definitive:
  `hyperd_running: false` is inconclusive; retry `status` for full
  statistics after the in-progress operation completes.

### Export
- `export` — write a table or query result to a file (Parquet, Iceberg,
  Arrow IPC, CSV, .hyper). A `.hyper` export leaves the source unchanged
  but creates/replaces the destination file and materializes every user
  table into it — a faithful backup: NOT NULL, DEFAULT, COLLATE, ASSUMED
  PRIMARY KEY, and ASSUMED UNIQUE all survive (Hyper never accepts
  enforced PRIMARY KEY / UNIQUE / FOREIGN KEY / CHECK at CREATE TABLE, so
  no source table carries those). The response carries a `schema_fidelity`
  object (`fully_preserved` plus per-class counts and an `unpreserved`
  list naming each `table` + `column`) — check it before trusting an
  export as a backup.
- `chart` — render a bar / line / scatter / histogram PNG or SVG from a
  SQL query as a quick diagnostic. Use long-format data (numeric y;
  optional `series` grouping). See `Chart delivery and presentation`.
- `copy_query` — run a SELECT across local + attached databases and
  insert the result into a target table (`mode`: `create`, `append`,
  `replace`). Cross-database analytics in one tool call.

### Saved queries & metadata
- `save_query` — save a named read-only SQL query for later reuse.
- `delete_query` — delete a named saved query.
- `set_table_metadata` — update prose metadata (source_url, purpose,
  notes, license, source_description) on an existing table catalog entry.
  Local and persistent tables share one name-keyed persistent catalog;
  writable user-attached databases have per-database catalogs.

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
  store + key. Returns `{stored, created, value_bytes}`. Pass
  `overwrite: false` to skip writes that would clobber an existing key
  (response: `{stored: false, existed: true}`). Pass
  `value_path: <absolute path>` to store a file's contents server-side
  instead of inlining `value` (exactly one of `value` / `value_path`;
  reads any path the server process can read — no sandbox; files over
  64 MiB are rejected before reading).
- `kv_set_many` — atomic batch write. Pass an `entries` array of
  `{key, value}` objects. All keys validated up front; an invalid key
  aborts the whole batch without writing anything. `overwrite: false`
  skips existing keys. Returns counts plus `total_bytes`, which counts
  submitted values and may exceed bytes persisted when entries skip.
- `kv_get` — read a value by store + key.
- `kv_delete` — delete a key.
- `kv_list` — list keys in a store. Pass `values: true` to return
  `{entries: [{key, value}, ...]}` instead of `{keys: [...]}` — reads
  the whole store in one call (eliminates N×`kv_get`).
- `kv_list_stores` — list store namespaces that hold data in a database.
- `kv_size` — count keys and total value bytes in a store. Returns
  `{size, bytes}`.
- `kv_pop` — destructively read-and-remove the lowest-keyed entry
  (lexicographic key order, not insertion order).
- `kv_clear` — delete all keys in a store.

Every kv_* tool takes the same optional `database` parameter as the data
tools. Omit it and the store lives in the local database (lost on
restart); pass `\"persistent\"` (or `persist: true`) to persist across
restarts, or any attached alias to target that database. Every user-attached
target must be writable, even for readers, because the backing table may
need initialization. The global `--read-only` guard blocks the five KV
mutators but not the four readers. Each database has its own isolated set
of stores. Enrich analytical tables with KV
metadata via LEFT JOIN — always filter `kv.store_name = '<namespace>'`
to avoid row multiplication, and keep the KV table in the same database
as the joined table. See the `hyper://schema/kv` resource for the join
template, and `KV store vs. a custom table` above for when to reach for
this instead of a real table.

**Querying JSON in a KV value:** Values are TEXT. To query JSON
structure, cast first: `SELECT value::json ->> 'field' FROM
_hyperdb_kv_store WHERE store_name = '...'`. The `->` / `->>` /
`JSON_EACH(...)` operators work AFTER the `::json` cast. Applying `->`
or `->>` to raw TEXT fails with SQLSTATE 42601 (\"requires a structured
data type\"). `JSON_VALUE(...)` is not implemented in this engine — use
the `::json` cast instead.

**::numeric truncation gotcha:** A bare `::numeric` cast defaults to
scale 0 and truncates decimal places. Example: `41.54178215::numeric`
→ `42`. Always specify precision and scale: `::numeric(20,10)`.

### Introspection
- `get_readme` — this document. Call once at the start of a session.

## Parameter rules

- **File paths must be absolute.** Relative paths are rejected.
- **Identifiers fold to lowercase** unless double-quoted. `SELECT * FROM
  Sales` reads `sales`. Use `\"Sales\"` to preserve case.
- **Query SQL is read-only** in every tool that takes it (`query`,
  `query_data`, `query_file`, `export`, `chart`, ...): one SELECT / WITH /
  EXPLAIN / SHOW / VALUES, even without `--read-only`. A write behind
  `WITH` or `EXPLAIN (ANALYZE)` is refused. For DDL / DML use `execute`.
- **Read-only mode** (`--read-only` flag on the server) guards exactly:
  `execute`, `load_data`, `load_file`, `load_files`, `load_iceberg`,
  `watch_directory`, `save_query`, `delete_query`, `set_table_metadata`,
  `copy_query`, `kv_set`, `kv_set_many`, `kv_delete`, `kv_pop`, and
  `kv_clear`. A writable `attach_database` or `on_missing: \"create\"` is
  also guarded, while a read-only attachment remains available.
  Queries and inspection, `chart`, and detach/list operations remain
  available. unwatch_directory remains allowed; export formats, including
  Hyper, remain allowed. Hyper export does not mutate its source database,
  but it does create or replace its materialized destination file.
- **Persistent-file contention:** only a reserved persistent attachment
  lock is reported as `RESOURCE_BUSY`. Run `hyperdb-mcp doctor`, compare
  client/daemon identities, close the possible owner (Hyper, Tableau, or
  another process), or copy/select another `.hyper` file, then retry.
- **Table names** in `load_*` and `query_data` / `query_file` accept
  unquoted identifiers; the server lowercases them.
- **`copy_query` modes:** `create` requires the target not exist;
  `append` requires it does; `replace` drops and recreates atomically.
- **`load_file` merge mode:** `mode = \"merge\"` requires `merge_key`
  (column name or list of column names). Rows whose key matches are
  deleted and replaced by the incoming row (target columns absent from
  the file become NULL; duplicate incoming keys all insert);
  non-matching rows INSERT. Columns present in
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
- **Physical `NullType` Parquet columns are unreadable** — what a writer
  emits for an all-null optional column in one partition. Hyper rejects
  the *whole file* with `42804` (\"a data type that cannot be read by
  Hyper ... hinting at a corrupted file\"); the file is not corrupt, and
  selecting only the other columns does not help. A `schema` override
  cannot fix it either: the override becomes a cast in the projection,
  which the engine never evaluates. Re-type the column at the writer
  (e.g. cast it to DOUBLE) and regenerate. `inspect_file` reports such a
  column as type `NULL`; `load_file`, `load_files`, and `query_file`
  fail fast and name it.
- **`to_char` is date/time only.** `to_char(TIMESTAMP '2020-01-02
  03:04:05', 'YYYY-MM-DD')` → `2020-01-02` and `to_char(DATE
  '2020-01-02', 'YYYY')` → `2020` both work. There is **no numeric
  overload** — integer and NUMERIC arguments alike fail with `42601
  unsupported data types in call to 'to_char'`, e.g.
  `to_char(123.456, 'FM990.00')`. That is an argument-type error, not a
  missing function: a function Hyper genuinely lacks returns `42883`, as
  `format()` does.
- **`expr::TEXT` is the numeric-formatting idiom** and preserves scale:
  `CAST(9.5 AS NUMERIC(8,2))::TEXT` → `9.50`. Reach for this instead of
  `to_char` on a number, and whenever you need exact decimal output.
- **A NUMERIC too large for an `f64` comes back as a JSON string**, not a
  number, so no precision is lost in the result: `99999999999999999.99`
  arrives as `\"99999999999999999.99\"`. Values that fit an `f64` — which
  includes anything with 15 or fewer significant digits — stay JSON
  numbers, so `9.50` arrives as `9.5`. Parse such a field as a decimal
  string rather than assuming a number, and note it may already be exact
  text if you applied `::TEXT` above.
- **`APPROX_COUNT_DISTINCT(expr)`** — approximate cardinality, 5-100x
  faster than `COUNT(DISTINCT ...)` at high cardinality, on TEXT as well
  as numeric keys. It accelerates the distinct step only, so a per-row
  `a || '-' || b` inside `expr` is paid either way and can swamp the win:
  prefer a numeric composite key (`a * 1000 + b`).
- **No `QUALIFY`** — absent from the grammar, so it fails the statement
  with SQLSTATE `42601` (syntax error; a missing *function* would be
  `42883`). Wrap the window in a subquery or CTE and filter outside. The
  rewrite is exactly equivalent and costs nothing measurable:
  ```
  SELECT * FROM (SELECT k, ROW_NUMBER() OVER (PARTITION BY p
    ORDER BY c DESC) AS rnk FROM t) s WHERE rnk <= 5
  ```
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
  A single-element array runs one auto-commit statement. Mixing DDL
  with DML in one batch is rejected — Hyper aborts
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
          FROM sales s JOIN lookup.public.dim_region d ON s.region = d.code \
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
// to refresh the table in place without dropping it.
load_file({
  \"path\": \"/tmp/extract_failures-with-host.json\",
  \"table\": \"extract_timing_failures\",
  \"mode\": \"merge\",
  \"merge_key\": \"job_id\"
})

// Single-statement execute (auto-commit)
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
  \"chart_type\": \"bar\",
  \"x\": \"region\",
  \"y\": \"total\"
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
