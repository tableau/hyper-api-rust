# Changelog

All notable changes to the `hyperdb-mcp` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [0.5.0] - 2026-06-07

### Fixed

- Query results now preserve the sign of negative `NUMERIC`/`DECIMAL` values
  with magnitude less than 1. Previously a value like `CAST(-0.5 AS
  numeric(10,4))` was serialized to JSON as `0.5` because `row_value_to_json`
  stringifies NUMERIC via `Numeric::to_string()`, whose `Display` impl dropped
  the sign for sub-unit magnitudes (fixed in `hyperdb-api-core`). This silently
  flipped the sign of correlations, 0–1 indices, and regression residuals.

### Added

- The `status` tool now reports an `engine` block with the backing `hyperd`
  connection: `mode` (`daemon` or `local`), `hyperd_endpoint` (the libpq
  endpoint queries run against), and `daemon_health_port` (the shared daemon's
  control/lock port, `null` in local mode).
- **Single-instance `hyperd` daemon** — by default, all MCP clients now
  share one `hyperd` process per user instead of each spawning their own.
  Multiple AI clients (Claude Code, Cursor, VS Code Copilot, etc.) can
  access the same persistent databases simultaneously with reduced
  resource overhead. The daemon auto-spawns on first client connect and
  stays resident (idle shutdown is opt-in — see below). Pass `--no-daemon`
  to opt out.
- **Identity-checked daemon discovery.** Clients verify a daemon by sending
  `PING` and requiring a `PONG hyperdb-mcp <version>` reply (matched on exact
  tokens, not a string prefix) before trusting it — a TCP connection alone is
  no longer sufficient, so an unrelated process occupying the port is no
  longer mistaken for the daemon.
- **Port scanning.** The daemon health/lock port now defaults to scanning
  upward from **7485** (16 ports), using the first free one; the old fixed
  default 7484 collided with `hyperd`'s conventional gRPC port. Set
  `HYPERDB_DAEMON_PORT` to pin an exact port (disables scanning). `daemon
  status` / `daemon stop` locate the daemon via discovery + scan, so they
  work regardless of which port it landed on.
- **Newer-client version takeover.** A starting client built from a strictly
  newer `hyperdb-mcp` version stops and replaces an older running daemon
  (and its `hyperd`), so upgrades take effect immediately instead of waiting
  for the old daemon to exit. Equal or older versions reuse the daemon.
- **Daemon stays resident by default.** Idle shutdown is now opt-in via
  `--idle-timeout <SECS>` or `HYPERDB_DAEMON_IDLE_TIMEOUT`; with neither set
  the daemon (and `hyperd`) stay warm, eliminating the connection error and
  "hyper restarting, please retry" round-trip a client previously hit after
  a 30-minute idle shutdown.
- New `daemon` subcommand: `hyperdb-mcp daemon status` / `daemon stop`.
- New environment variables: `HYPERDB_STATE_DIR`, `HYPERDB_DAEMON_PORT`,
  `HYPERDB_DAEMON_IDLE_TIMEOUT`.
- Ephemeral databases now `DETACH DATABASE` before deletion on session
  end — required on Windows where the OS enforces file locks on open
  Hyper files.
- **Daemon-side `hyperd` restart on crash.** The daemon polls `hyperd`
  every 5 seconds via `Child::try_wait()` and automatically restarts it
  if the process has exited, atomically updating the discovery file
  with the new endpoint. Clients reconnect transparently via the
  existing `ConnectionLost` recovery path. New `REPORT_HYPERD_ERROR`
  health-protocol command lets clients fast-path the signal when they
  detect a dead hyperd before the daemon's polling tick. Restart
  attempts are rate-limited to 3 per 60 seconds; exceeding the limit
  triggers daemon shutdown so the user sees the failure clearly
  rather than spinning silently.
- **Two-database engine model.** Every session now has both an
  ephemeral primary (created fresh per-session, deleted on exit) AND
  a persistent attachment under the alias `"persistent"`. Unqualified
  SQL routes to the ephemeral primary (the LLM's scratch space);
  fully-qualified SQL like
  `INSERT INTO "persistent"."public"."customers" ...` writes to
  storage that survives across sessions.
- **Platform-default persistent path.** When `--persistent-db` is
  unset, the persistent file lives at the platform data directory:
  `~/Library/Application Support/hyperdb/workspace.hyper` on macOS,
  `~/.local/share/hyperdb/workspace.hyper` on Linux,
  `%APPDATA%\hyperdb\workspace.hyper` on Windows. Override with
  `HYPERDB_PERSISTENT_DB`.
- **New CLI flags:** `--persistent-db <PATH>` (replaces `--workspace`,
  which is kept as a deprecated alias with a stderr warning); and
  `--ephemeral-only` to skip the persistent attachment entirely.
- The `_table_catalog` and `_hyperdb_saved_queries` meta-tables now
  live in the persistent attachment instead of the connection's
  primary, so saved queries automatically persist across sessions
  without any flag toggling.
- **Per-tool `database` parameter.** `query`, `execute`, `load_data`,
  `load_file`, `describe`, `sample`, `chart`, and `export` now accept
  an optional `database: string` parameter. Omit (or pass `"local"`)
  to target the ephemeral primary; pass `"persistent"` to target the
  durable database (case-insensitive), or any user-attached writable
  alias. Tools that build their own SQL fully qualify table
  references; tools that take user-provided SQL temporarily redirect
  `schema_search_path` for the call duration via an RAII guard that
  always restores on drop. (`query_data` and `query_file` materialize
  inline data into a temp table; they don't accept the parameter.)
- **`persist: true` shorthand on ingest tools.** `load_data` and
  `load_file` accept a boolean `persist` flag — equivalent to
  `database: "persistent"` — for ergonomic LLM workflows like
  `load_data({ table: "x", data: "[...]", persist: true })`. If
  both `database` and `persist` are set, `database` wins. Combining
  `persist: true` with `--ephemeral-only` returns a clear
  `InvalidArgument` error.
- **Cross-database merge.** `load_file` with `mode: "merge"` now
  accepts any writable `database`. The merge path keeps the temp
  table inside the target database so DELETE-USING and INSERT-SELECT
  stay single-DB — no cross-database DML is required. Engine
  helpers `table_exists_in`, `column_metadata_in`, and
  `alter_table_add_columns_in` carry the routing.
- **`export(format="hyper", database=...)`.** The hyper-format
  export now snapshots whichever database the caller named (via a
  new `ExportOptions.source_db` field plumbed into
  `populate_export_target`). Default behavior (snapshot primary)
  unchanged.
- **`load_files` and `watch_directory` accept `database` / `persist`.**
  Their connection pool now opens the resolved target's `.hyper`
  file directly as its workspace, so unqualified ingest SQL routes
  into the right database without further plumbing. The watcher's
  reconnect-recovery path re-resolves the target so a hyperd
  restart picks the right file. `WatcherHandle` records its
  `target_db`; `detach_database` rejects with `InvalidArgument` if
  any active watcher targets the alias (call `unwatch_directory`
  first to release it).
- **Per-database `_table_catalog`.** Every writable database
  receives its own catalog, lazily seeded on first ingest. The
  catalog CRUD API gains `*_in(target_db)` siblings
  (`ensure_exists_in`, `upsert_stub_in`, `set_metadata_in`,
  `get_in`, `list_in`, `delete_for_in`, `reconcile_in`). The
  per-engine catalog-presence cache is now keyed by canonical
  alias (`Mutex<HashMap<String, bool>>`); detach clears the entry.
- **`set_table_metadata.database` parameter.** Routes the catalog
  write to the named database's `_table_catalog`. Read-only
  attachments are rejected up front with a clear "re-attach with
  writable:true" message.

### Changed (breaking — pre-1.0)

- `Engine::catalog_present_in_persistent` → `Engine::catalog_present_in(alias, prober)`.
- `Engine::mark_catalog_present` → `Engine::mark_catalog_present_for(alias)`.
- `Engine::catalog_present_cache` field shape: `Mutex<Option<bool>>` → `Mutex<HashMap<String, bool>>` keyed by lowercased alias.
- New `Engine::clear_catalog_cache_for(alias)` paired with `detach_database`.
- `table_catalog::ensure_exists_in_database(engine, alias)` is now a deprecated wrapper over `ensure_exists_in(engine, Some(alias))`.
- **Attach aliases are canonicalized to lowercase at attach time.**
  `attach_database(alias="MyDB", …)` now stores `"mydb"` in the registry,
  and `Engine::resolve_target_db` returns the lowercase form for any
  alias. Eliminates the latent footgun where attaching as `"User_DB"`
  and detaching as `"user_db"` silently no-op'd while leaving the
  catalog-presence cache populated. Affects users who relied on
  case-sensitive registry distinctness — pre-1.0, no migration is
  shipped.

### Removed

- **`--bare` flag.** Catalog seeding is now uniform: created when MCP
  creates a fresh `.hyper`, never touched on existing files. Users
  who want a pristine `.hyper` for export can `DROP TABLE _table_catalog`
  once after creation; subsequent opens won't recreate it.

### Performance

- **Per-database `_table_catalog` presence cache.** Catalog reads/writes
  used to round-trip a `pg_catalog.pg_tables` probe on every call;
  now the existence check is cached per (engine, alias) and primed
  immediately after `CREATE TABLE IF NOT EXISTS`. `detach_database`
  clears the alias's entry so a re-attach to a different file isn't
  served stale.
- **Cross-process catalog write safety via optimistic concurrency.**
  `upsert_stub_in` now uses UPDATE-then-conditional-INSERT instead
  of DELETE+INSERT in a transaction. Each statement is individually
  atomic at the `hyperd` level, so multiple MCP server processes
  sharing the same persistent database via the daemon can no longer
  produce duplicate `_table_catalog` rows from concurrent ingests.
  The UPDATE path also eliminates one round-trip (the pre-read of the
  existing row to preserve prose fields — UPDATE preserves them
  implicitly by only touching mechanical columns).

### Fixed

- **Chart x-axis tick label thinning.** Long categorical line/scatter
  charts (e.g. a 90-point hourly TIMESTAMP series) used to render with
  only ONE visible x-axis label. The old logic blanked individual
  labels at non-step indices, but `plotters` picks its own tick
  *positions* on the float axis and rounds them to integer indices —
  so almost none of the chosen ticks landed on a kept index, and the
  formatter returned empty strings for the rest. The chart layer now
  computes a target tick count from chart width and label sizes and
  passes that count to `plotters` via `.x_labels(N)`; every drawn
  tick carries its real label. Same `+00:00` suffix stripping for
  shared TIMESTAMPTZ offsets is preserved (now isolated in
  `strip_shared_tz_suffix`).
- **Line / scatter charts over `DATE`, `TIMESTAMP`, and `TIMESTAMPTZ`
  columns now use a proportional time axis** instead of the previous
  categorical-with-evenly-spaced-ticks behavior. Real-world time gaps
  between data points are now reflected in the chart's x-axis: a
  series at `2026-05-01 08:00`, `2026-05-01 12:30`, `2026-05-02 06:15`
  shows the 4.5-hour and 17.75-hour gaps proportionally instead of
  flattening every interval to the same width. Tick labels are
  formatted via `chrono` in a form that matches the input kind:
  `%Y-%m-%d` for DATE, `%Y-%m-%d %H:%M:%S` for TIMESTAMP, and
  `%Y-%m-%d %H:%M:%S%:z` for TIMESTAMPTZ (the offset captured from the
  first row, so a uniformly-`+05:30` series reports IST throughout).
  Set `x_as_category: true` to opt out and force the previous
  categorical layout (e.g. for charts where evenly-spaced bins are
  more readable than proportional gaps). TEXT x columns continue to
  render categorically as before. Bar charts are unaffected — they
  remain categorical regardless of x type, which matches reader
  expectations for grouped data.
- **Watcher recovery after hyperd restart.** The watcher's connection
  pool now auto-rebuilds when a per-file ingest hits a connection-lost
  error (typically after the daemon restarts hyperd). Each ingest gets
  one retry on a fresh pool; persistent failures still flow into the
  standard `failed/` move so a single broken file can't pin the
  watcher in retry loops.
- **`execute` now reconciles the user-attached target's `_table_catalog`.**
  Pre-fix, raw DDL like `execute(database="foo", sql="DROP TABLE bar")`
  removed the table from the user-attached DB but left its stub row
  stranded in `"foo"."public"."_table_catalog"` indefinitely (bootstrap
  reconcile and the post-execute reconcile both walked persistent
  only). `after_execute_catalog_update` now reconciles persistent
  first and then the user-attached target if non-persistent. Gated on
  `is_structural_sql` (CREATE / DROP / ALTER / TRUNCATE / RENAME) so
  per-row `INSERT` / `UPDATE` / `DELETE` no longer triggers a
  workspace-wide catalog scan on every call.
- **`copy_query.target_database` now canonicalizes mixed-case aliases.**
  Pre-fix, `attach_database(alias="My_DB")` (which the registry stores
  as `"my_db"` after the alias-canonicalization change) followed by
  `copy_query(target_database="My_DB")` rendered SQL referring to
  `"My_DB"` and Hyper rejected it with "database does not exist"
  (Hyper is case-sensitive on quoted identifiers). The tool now
  lowercases `target_database` after the `LOCAL_ALIAS` filter so the
  registry lookup AND the qualified-SQL build path agree on the
  canonical lowercase form.

## [0.1.1] - 2026-05-13

### Added

MCP tools — query and execute:

- `query` — read-only SELECT / WITH / EXPLAIN / SHOW / VALUES
- `execute` — DDL / DML (CREATE, INSERT, UPDATE, DELETE, DROP, etc.)
- `query_data` — ingest inline JSON or CSV and run one SQL query
- `query_file` — same as `query_data` but reads from a file path

MCP tools — load:

- `load_file` — load one CSV / JSON / JSONL / Parquet / Arrow IPC file into a workspace table
- `load_files` — load many files in parallel
- `load_data` — load inline JSON or CSV into a named workspace table
- `load_iceberg` — load an Apache Iceberg table by absolute root-directory path

MCP tools — inspect:

- `describe` — list workspace tables or describe one table
- `sample` — return schema + first N rows of a table
- `inspect_file` — dry-run schema inference on a file before loading
- `status` — plugin health, workspace path, table count, total rows, disk usage, watchers, attached databases, read-only flag

MCP tools — export and visualize:

- `export` — write a table or query result to a file (Parquet, Iceberg, Arrow IPC, CSV, `.hyper`)
- `chart` — render PNG/SVG charts (bar, line, scatter, histogram) from a SQL query via `plotters`
- `copy_query` — run a SELECT across local + attached databases and insert the result into a target workspace table (`create` / `append` / `replace` modes)

MCP tools — saved queries:

- `save_query` — save a named read-only SQL query for later reuse, exposed as MCP resources
- `delete_query` — delete a previously saved query

MCP tools — table metadata:

- `set_table_metadata` — update prose metadata (source URL, purpose, etc.) on a workspace table

MCP tools — multi-database:

- `attach_database` — attach an additional `.hyper` database under an alias for cross-database JOINs
- `detach_database` — detach a previously attached database
- `list_attached_databases` — list current attachments

MCP tools — directory watch:

- `watch_directory` — auto-ingest matching files via `.ready` sentinel files
- `unwatch_directory` — stop watching a previously registered directory

MCP tools — introspection:

- `get_readme` — return the LLM-facing README for orientation at the start of a session

MCP prompts:

- `analyze-table` — guided table analysis prompt
- `compare-tables` — side-by-side table comparison prompt
- `data-quality` — data-quality assessment prompt
- `suggest-queries` — query suggestion prompt

Library modules:

- `attach` — registry of attached `.hyper` databases for cross-database JOINs
- `chart` — chart rendering via `plotters`
- `engine` — `HyperProcess` lifecycle, connection management, table CRUD, query execution
- `error` — structured error codes with LLM-friendly recovery suggestions
- `export` — write query results to CSV, Parquet, Arrow IPC, or `.hyper` files
- `ingest` — inline JSON and CSV loading via `INSERT` and `COPY`
- `ingest_arrow` — Parquet and Arrow IPC file loading
- `inspect` — dry-run file inspection powering the `inspect_file` tool
- `lakehouse` — Apache Iceberg ingest via `hyperd`'s native external-format reader
- `readme` — static LLM-facing README returned by `get_readme`
- `saved_queries` — named read-only SQL with persistence
- `schema` — three-tier schema inference (exact / structural / heuristic) with user overrides
- `server` — MCP tool definitions and `rmcp` server handler
- `stats` — performance telemetry (throughput, timing) on responses
- `subscriptions` — MCP resource-update notifications
- `table_catalog` — user-visible table metadata (`_table_catalog` tracking)
- `version` — compile-time version strings with git-hash suffix
- `watcher` — directory monitoring for incremental ingest via `.ready` sentinels

Other:

- `hyperdb-mcp` CLI binary for invoking the MCP server (workspace, ephemeral, read-only modes)
- Zero feature flags — all capabilities always available
