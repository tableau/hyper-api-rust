// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! LLM-facing README returned by the `get_readme` tool.
//!
//! Written in a compact pseudo-code / schema notation (legend at the top of
//! the string): tool signatures as `tool(required, optional?=default) →
//! result`, `a|b` alternatives, `*` defaults, and terse fragments instead of
//! prose. The notation buys room for more facts within the byte budget that
//! `readme_tests.rs` enforces. Order: purpose → database model → chart →
//! tool index → parameter rules → SQL quirks → examples → tips. When tools
//! or features change, update this string; `readme_tests.rs` fails loudly if
//! a tool name or a pinned contract phrase goes missing.

pub const README: &str = r##"# HyperDB MCP

In-process SQL analytics on the Tableau Hyper engine. In: CSV|JSON|JSONL|Parquet|Arrow IPC|Apache Iceberg.
SQL: PostgreSQL-compatible, Salesforce Data Cloud dialect. Out: Parquet|Iceberg|Arrow IPC|CSV|.hyper.
Use for any tabular analysis, SQL, file transform, or chart; prefer over ad-hoc Python/shell (faster
parsing, native SQL, state stays re-queryable without re-loading).

Notation: `tool`(required, optional?) → result · `a|b` one of · `*` or `?=v` default · `≡` same as · ✓ supported ·
✗ unsupported · ⚠ gotcha. File paths absolute; relative rejected.

## Database model — queryable memory

```
local*: ephemeral (fresh per session, deleted on exit); unqualified SQL lands here; scratch data
persistent: survives sessions = long-term structured memory (reference data, preferences, accumulated
  results, anything to remember or query later); off under --ephemeral-only
<alias>: attach_database → user-chosen name for its own .hyper file, canonical lowercase alias; read-only unless writable: true
remember: few scraps (variables, flags, summaries, JSON, queue entries) → kv_*, no schema/DDL · rows to
  filter/JOIN/aggregate → table (load_data | execute CREATE TABLE) · both local (lost on restart)
  unless database: "persistent"
database: "local"* | "persistent" | <alias>, case-insensitive; writes need a writable attachment. On:
  query execute load_data load_file load_files watch_directory describe sample chart export
  set_table_metadata kv_*
persist: true ≡ database: "persistent" (database wins if both set). On: load_data load_file load_files watch_directory kv_*
execute: database only · load_iceberg: always local · copy_query: target_database
fully-qualified SQL (power users; prefer database where a tool takes it):
  INSERT INTO "persistent"."public"."customers" SELECT ...
e.g. load_data({table: "project_decisions", data: "[...]", persist: true})
     query({sql: "SELECT * FROM project_decisions", database: "persistent"})
```

### Chart delivery and presentation

`chart`(sql, chart_type: bar|line|scatter|histogram, x?, y?, series?, title?, format?: png|svg (else
from output_path extension, else png),
width?=800, height?=480, bins?=20, ...) — a bounded quick diagnostic, not a dashboard system.
Long-format data: numeric y, optional series. x, y required for bar|line|scatter; histogram bins x.
```
delivery: no output_path → PNG (default) or SVG returned inline, no file · output_path → file written
  (parents created) and returned inline · inline=false → disk only (no path ⇒ auto-generated temp
  file) · existing file → PERMISSION_DENIED unless overwrite=true; an open database is never replaced ·
  explicit format and path extension, if both given, must agree (.png|.svg only)
database: routes the SQL to local|persistent|attached
size: width 200-4096, height 150-4096 (clamped). Long and Unicode labels are not truncated or
  auto-sized; if they clip, increase width or height
bar_orientation: vertical* | horizontal (rankings); bars only
label_values=true: each original y scalar written beside its bar
show_legend: true by default; false to suppress the legend
color_map: {series name → "#rrggbb"}, needs series; unlisted series use the default palette
label_points=true: labels line/scatter points, suppresses their legend
y_scale: linear (default) | log, on the data-role y measure (horizontal bars: physical x axis). Log
  values and ranges: finite, strictly positive (> 0); zero and negative invalid; logarithmic
  histograms unsupported; an explicit range must contain every plotted value. Log bars start at the
  positive lower bound, not zero
x_range, y_range: [min, max]; finite, strictly increasing, representable; omit to auto-scale
x_range: line|scatter → axis bounds only; out-of-range points still counted in rows_plotted, rendered
  invisibly (clipped), never moved · histogram → binning extent; outside values excluded from the bins
  (not folded into edge bins), rows_plotted counts binned values only, stats.excluded_out_of_range =
  dropped count, an x_range containing no data renders empty at that extent (no error) · bar → ignored (categorical x)
bins: clamped 1..=500
x axis: line|scatter DATE|TIMESTAMP|TIMESTAMPTZ → proportional time axis (automatic) · numeric →
  numeric · TEXT → categorical · x_as_category=true → even spacing for temporal observations · bars
  always categorical
```

Every successful database-routed tool response carries canonical `resolved_database`: "local",
"persistent", or the lowercase attached alias. `copy_query` also retains `target_database`.

## Tool index

### Query
- `query`(sql, database?) → rows; read-only SELECT|WITH|EXPLAIN|SHOW|VALUES; max 10,000 rows (check
  `truncated`, `total_rows`)
- `execute`(sql: [stmt, ...], database?) — DDL/DML; several elements = one transaction (all commit or
  all roll back), one = auto-commit. Disabled in read-only mode
- `query_data`(data, sql, format?: json|csv, table_name?="data", schema?) — inline JSON array or CSV +
  one query, temporary table; format auto-detected (`[`/`{` → JSON, else CSV)
- `query_file`(path, sql, table_name?="data", schema?, json_extract_path?) — same for a CSV|JSON|JSONL|
  Parquet|Arrow IPC file; fastest answer to "what's in this file?"
- `schema`: partial override `{"col": "BIGINT"}`, rest inferred · `json_extract_path`: dot path to a
  nested JSON array, numeric segments index arrays (`content.0`), stringified JSON parsed en route

### Load
Format preference (ingest and export): Parquet (fastest, server-side, keeps every type incl. NUMERIC
precision/DATE/TIMESTAMP; large data) > Arrow IPC (very fast, uncompressed; authoritative schema, so
overrides rejected) > CSV (portable; types inferred on load, lost on export) > JSON|JSONL (small or
irregular only; row-by-row). Iceberg: directory of Parquet, data-lake interop. `.hyper` export:
snapshot of every table, for Tableau.
- `load_file`(path, table, mode?: replace*|append|merge, merge_key?, schema?, json_extract_path?,
  database?, persist?) — one CSV|JSON|JSONL|Parquet|Arrow IPC file → table. merge: upsert by
  `merge_key` (column or list); new incoming columns auto-added via `ALTER TABLE` (Parameter rules)
- `load_files`(files: [{path, table, mode?: replace*|append, schema?, json_extract_path?}],
  concurrency?, database?, persist?) — parallel, one table each. ⚠ No merge: `load_file` per file
- `load_data`(table, data, format?: json|csv, mode?: replace*|append, schema?, database?, persist?) —
  inline JSON/CSV → table; replace drops and recreates
- `load_iceberg`(path, table, mode?: replace*|append, metadata_filename?, version_as_of?) — Apache
  Iceberg table root directory (`metadata/`, `data/`); pin a snapshot by `metadata_filename` (e.g.
  "v2.metadata.json"; latest*) or `version_as_of`

### Inspect
- `describe`(table?, database?) — no table → list tables; table → columns, types, row count, prose
  metadata. ⚠ Local by default: pass `database: "persistent"` (or an alias) for durable tables;
  `status` gives table counts, never names, so check here before assuming a table is missing
- `sample`(table, n?, database?) → schema + first N rows; use before a non-trivial query
- `inspect_file`(path, sample_rows?, json_extract_path?) — dry-run schema inference on a CSV|Parquet|
  Arrow IPC|JSON file, no load
- `status`() → plugin and native/API identity; daemon/Hyper connection facts; local/persistent paths;
  table count; disk usage; watchers; attachments; read-only flag; `default_database: "local"` (full or
  degraded). `engine.connection`, the `hyperd` endpoint for another Hyper client: `transport`
  tcp|unix_domain_socket|named_pipe · `host`, `port` (TCP only, else null) · `socket_path` (IPC only,
  else null) · `connection_descriptor`, the scheme-qualified string `hyperd` emits and the Hyper API
  accepts (`tab.tcp://host:port`). Shared daemon → IPC; private `hyperd` (`--no-daemon` or daemon
  fallback) → TCP. ⚠ `engine_busy: true` → partial, non-definitive response; `hyperd_running: false`
  is inconclusive; retry `status` for full statistics after the in-progress operation completes

### Export
- `export`(path, format: parquet|iceberg|arrow_ipc|csv|hyper, sql? | table?, overwrite?, format_options?,
  database?) — table or query result → file (iceberg: directory); `sql` or `table` required except
  hyper (exports everything, ignores both); both given → `sql` wins. Existing destination →
  PERMISSION_DENIED unless `overwrite: true`; iceberg replaces only an Iceberg table directory, hyper
  only a `.hyper` file, never an open database. `database` = source; sql-mode unqualified names resolve
  there. `.hyper` export leaves the source unchanged and writes every user table to the destination, a
  faithful backup: NOT NULL, DEFAULT, COLLATE, ASSUMED PRIMARY KEY, ASSUMED UNIQUE survive (Hyper never
  accepts enforced PRIMARY KEY/UNIQUE/FOREIGN KEY/CHECK at CREATE TABLE, so no source table has them).
  Check `schema_fidelity` {`fully_preserved`, per-class counts, `unpreserved`: [{`table`, `column`}]}
  before trusting an export as a backup
- `format_options` → hyperd `COPY ... WITH (...)`; exact hyperd names, scalar values; ignored for hyper.
  parquet: `codec` snappy*|zstd|gzip|uncompressed|…, `rows_per_row_group` · iceberg: parquet's +
  `table_scheme` metastore*|filesystem, `max_file_size` (bytes) · csv: `header` (true*), `delimiter`
  (","*), `null` (""*), `quote`
- `chart` — see Chart delivery and presentation
- `copy_query`(sql, target_table, mode: create|append|replace, target_database?, temp_attach?:
  [attach_database args]) — SELECT|WITH|VALUES across local + attached (qualified: `src.public.t`) →
  `public` schema of `target_database` (local*; else writable). `temp_attach`: this call only,
  auto-detached even on failure; aliases must be unused

### Saved queries & metadata
- `save_query`(name, sql, description?) — named read-only query → resources
  `hyper://queries/{name}/definition`, `.../result` (re-runs on read); survives restarts unless
  `--ephemeral-only`; duplicate name rejected (`delete_query` first)
- `delete_query`(name) — missing name: no-op → `{"deleted": false}`
- `set_table_metadata`(table, source_url?, purpose?, notes?, license?, source_description?, data_url?,
  database?) — prose metadata on an existing table catalog entry; omitted unchanged, "" clears. Local
  and persistent tables share one name-keyed persistent catalog; writable user-attached databases have
  per-database catalogs

### Multi-database
- `attach_database`(alias, path, writable?=false, on_missing?: error*|create, kind?: local_file*) —
  alias: SQL identifier, not `local`. `writable: true` allows writes through it. `on_missing: "create"`
  makes an empty file (needs `writable: true`; parent directory must exist)
- `detach_database`(alias) · `list_attached_databases`() → current attachments

### Directory watching
- `watch_directory`(path, table, max_concurrent?=4 (≤32), database?, persist?) — producer writes
  `x.csv` atomically (tmp + rename), then empty `x.csv.ready`; watcher appends x.csv to `table`,
  deletes both on success; on failure moves both to `failed/` + `x.csv.error` JSON (no retry)
- `unwatch_directory`(path) — stop

### Key-value store (scratchpad)
- `kv_set`(store, key, value | value_path, overwrite?=true) → `{stored, created, value_bytes}`;
  `overwrite: false` + existing key → `{stored: false, existed: true}`. `value_path`: absolute path,
  contents stored server-side instead of `value` (exactly one of the two; any path the server process
  can read, no sandbox; over 64 MiB rejected before reading)
- `kv_set_many`(store, entries: [{key, value}], overwrite?=true) — atomic batch; keys validated up
  front, one invalid key aborts it, nothing written; `overwrite: false` skips existing keys →
  `{stored, created, overwritten|skipped, total_bytes}` (submitted values; may exceed bytes persisted
  when entries skip)
- `kv_get`(store, key) → `{found, value}` · `kv_delete`(store, key) · `kv_clear`(store) — delete all keys
- `kv_list`(store, values?) → `{keys: [...]}`; `values: true` → `{entries: [{key, value}, ...]}`, whole
  store in one call (no N×`kv_get`)
- `kv_list_stores`() → store namespaces holding data in a database · `kv_size`(store) → `{size, bytes}`
- `kv_pop`(store) → `{found, key, value}` — destructive read-and-remove of the lowest-keyed entry (lexicographic key order, not
  insertion order)

Stores appear on first write. Every kv_* tool also takes `database?`/`persist?`: local* (lost on
restart); "persistent" or `persist: true` survives; an attached alias targets that database. Every
user-attached target must be writable, even for readers (the backing table may need initialization).
`--read-only` blocks the five KV mutators, not the four readers. Each database has its own isolated
stores. Enrich tables via LEFT JOIN: always filter `kv.store_name = '<namespace>'` (else rows
multiply); keep the KV table in the joined table's database; template: `hyper://schema/kv` resource.

**Querying JSON in a KV value:** values are TEXT; cast first: `SELECT value::json ->> 'field' FROM
_hyperdb_kv_store WHERE store_name = '...'`. `->`, `->>`, `JSON_EACH(...)` work after `::json`; on raw
TEXT → SQLSTATE 42601 ("requires a structured data type"). `JSON_VALUE(...)` not implemented; cast.

**`::numeric` gotcha:** bare `::numeric` is scale 0, drops decimals: `41.54178215::numeric` → `42`.
Give precision and scale: `::numeric(20,10)`.

### Introspection
- `get_readme`() — this document; call once at session start

## Parameter rules

- Identifiers fold to lowercase unless double-quoted: `SELECT * FROM Sales` reads `sales`; `"Sales"`
  keeps case. Table names in `load_*`, `query_data`, `query_file`: unquoted identifiers, lowercased.
- Query SQL is read-only in every tool taking it (`query`, `query_data`, `query_file`, `export`,
  `chart`, `save_query`, ...): one SELECT|WITH|EXPLAIN|SHOW|VALUES, even without `--read-only`; a write
  behind `WITH` or `EXPLAIN (ANALYZE)` is refused. DDL/DML → `execute`.
- `--read-only` server flag guards exactly: execute, load_data, load_file, load_files, load_iceberg,
  watch_directory, save_query, delete_query, set_table_metadata, copy_query, kv_set, kv_set_many,
  kv_delete, kv_pop, kv_clear, plus a writable `attach_database` or `on_missing: "create"`. Read-only
  attachments, queries, inspection, `chart`, detach/list stay available.
  unwatch_directory remains allowed; export formats, including Hyper, remain allowed. Hyper export does not mutate its source
  database, but it does create or replace its materialized destination file.
- Persistent-file contention: only a reserved persistent attachment lock is reported as
  `RESOURCE_BUSY`. Run `hyperdb-mcp doctor`, compare client/daemon identities, close the possible owner
  (Hyper, Tableau, or another process) or copy/select another `.hyper` file, then retry.
- `copy_query` mode: create (target must not exist; CREATE TABLE AS) | append (must exist; INSERT
  INTO ... SELECT) | replace (drop and recreate, atomically).
- `load_file` merge (needs `merge_key`): key match → row deleted, incoming row inserted (target columns
  absent from the file → NULL; duplicate incoming keys all insert) · no match → INSERT · new file
  column → `ALTER TABLE ADD COLUMN` (nullable, existing rows NULL) · type change on an existing column
  → rejected (use replace or a `schema` override). ⚠ DELETE+INSERT is not transactional (Hyper
  auto-commits DDL): a mid-run failure leaves partial state, same as replace.

## SQL dialect quick-reference

PostgreSQL-compatible plus Salesforce Data Cloud SQL extensions. Differences:

- ✗ `information_schema`, `pg_catalog` → `describe`/`sample`. ✗ JSON, JSONB, UUID, SERIAL, BIGSERIAL,
  geometry. Atomic types only: SMALLINT, INTEGER, BIGINT, REAL, DOUBLE PRECISION, NUMERIC(p,s),
  BOOLEAN, TEXT, CHAR(n), VARCHAR(n), BYTES, DATE, TIME, TIMESTAMP, TIMESTAMPTZ, INTERVAL, + arrays of
  any atomic type.
- ✓ `external(path, format => '...')` in FROM: read Parquet|CSV|Iceberg from disk, no load.
- ⚠ Physical `NullType` Parquet column (a writer's all-null optional column in one partition) → Hyper
  rejects the whole file, `42804` ("a data type that cannot be read by Hyper ... hinting at a corrupted
  file"). Not corrupt; selecting other columns doesn't help; a `schema` override can't fix it (it
  becomes a projection cast the engine never evaluates). Re-type it at the writer (e.g. cast to
  DOUBLE), regenerate. `inspect_file` shows type `NULL`; `load_file`, `load_files`, `query_file` fail
  fast naming it.
- ⚠ `to_char` is date/time only: `to_char(TIMESTAMP '2020-01-02 03:04:05', 'YYYY-MM-DD')` →
  `2020-01-02`, `to_char(DATE '2020-01-02', 'YYYY')` → `2020`. No numeric overload: integer and
  NUMERIC alike → `42601 unsupported data types in call to 'to_char'` (e.g. `to_char(123.456,
  'FM990.00')`), an argument-type error; a truly missing function is `42883`, like `format()`.
- ✓ `expr::TEXT` formats numbers, keeps scale: `CAST(9.5 AS NUMERIC(8,2))::TEXT` → `9.50`. Use it
  instead of `to_char` on numbers and for exact decimal output.
- ⚠ NUMERIC too large for `f64` → JSON string, no precision lost: `99999999999999999.99` →
  `"99999999999999999.99"`. Fits `f64` (any ≤15 significant digits) → JSON number: `9.50` → `9.5`.
  Parse such fields as decimal strings; after `::TEXT` they are exact text.
- ✓ `APPROX_COUNT_DISTINCT(expr)`: approximate cardinality, 5-100x faster than `COUNT(DISTINCT ...)`
  at high cardinality, TEXT or numeric. Speeds only the distinct step: per-row `a || '-' || b` in
  `expr` is paid either way and can swamp the win; prefer a numeric key (`a * 1000 + b`).
- ✗ `QUALIFY`: not in the grammar → `42601` (syntax; a missing function would be `42883`). Filter a
  subquery/CTE instead (exactly equivalent, no measurable cost): `SELECT * FROM (SELECT k,
  ROW_NUMBER() OVER (PARTITION BY p ORDER BY c DESC) AS rnk FROM t) s WHERE rnk <= 5`
- ✓ All standard window functions + `modified_rank()` (`rank()` but LOWEST rank on ties); `IGNORE
  NULLS`/`RESPECT NULLS` only on `last_value`.
- ✓ `DISTINCT ON (expr, ...)`, `GROUPING SETS`, `ROLLUP`, `CUBE`, `FILTER (WHERE ...)`, ordered-set
  aggregates (`MODE()`, `PERCENTILE_CONT()`, `PERCENTILE_DISC()` + `WITHIN GROUP`), `WITH`, `WITH
  RECURSIVE` (each CTE evaluated once per query), `TOP N` alongside `LIMIT`.
- ✗ AI scalar functions (`AI_CLASSIFY`, `AI_SENTIMENT`, ...): Data Cloud federation, not Hyper.
- ✗ `ON CONFLICT`, `INSERT ... ON DUPLICATE KEY` → statement array to `execute`, run atomically (see
  the upsert example). ⚠ DDL + DML in one batch rejected (Hyper aborts it, SQLSTATE 0A000): DDL in its
  own `execute`. ⚠ No `BEGIN`/`COMMIT`/`ROLLBACK`/`SAVEPOINT` elements: the tool manages the
  transaction and rejects them up front.

Full reference: https://developer.salesforce.com/docs/data/data-cloud-query-guide/references/dc-sql-reference

## Examples

```
// Quickest path: query a file without loading it as a table
query_file({ "path": "/tmp/sales.csv", "sql": "SELECT region, SUM(amount) FROM data GROUP BY region" })
// Inspect a file before committing to a load
inspect_file({ "path": "/tmp/sales.csv" })
// Loaded-table workflow (best for multiple queries)
load_file({ "path": "/tmp/sales.csv", "table": "sales" }); sample({ "table": "sales" })
query({ "sql": "SELECT region, SUM(amount) FROM sales GROUP BY region" })
// Cross-database join via attachment
attach_database({ "alias": "lookup", "path": "/data/dim.hyper" })
query({ "sql": "SELECT s.region, d.country_name, SUM(s.amount) FROM sales s
  JOIN lookup.public.dim_region d ON s.region = d.code GROUP BY s.region, d.country_name" })
// Read Parquet directly inside a query, no load step
query({ "sql": "SELECT COUNT(*) FROM external('/tmp/events.parquet', format => 'parquet')" })
// Export a query result
export({ "sql": "SELECT * FROM sales WHERE amount > 1000", "path": "/tmp/big.parquet", "format": "parquet" })
// Re-parsed source gained fields: upsert by job_id, auto-add columns, refresh in place, no drop
load_file({ "path": "/tmp/failures.json", "table": "failures", "mode": "merge", "merge_key": "job_id" })
// Single-statement execute (auto-commit)
execute({ "sql": ["DELETE FROM events WHERE created_at < CURRENT_DATE - INTERVAL '90' DAY"] })
// Atomic upsert: both statements commit together or both roll back
execute({ "database": "persistent", "sql": [
  "UPDATE settings SET value = 'dark' WHERE key = 'theme'",
  "INSERT INTO settings (key, value) SELECT 'theme', 'dark'
     WHERE NOT EXISTS (SELECT 1 FROM settings WHERE key = 'theme')" ] })
// Chart
chart({ "sql": "SELECT region, SUM(amount) AS total FROM sales GROUP BY region",
  "chart_type": "bar", "x": "region", "y": "total" })
```

## Tips for picking the right tool

- one-shot "what's in this file?" → `query_file` or `inspect_file`
- repeated analysis of the same data → `load_file` once, then `query`
- file too large for memory → `external()` inside `query` (streams from disk)
- join across .hyper files → `attach_database` + `query`
- materialize a query result as a table → `copy_query`
- a picture for the user → `chart`
- re-parsed source with new columns, update in place → `load_file` with `mode: "merge"`
"##;
