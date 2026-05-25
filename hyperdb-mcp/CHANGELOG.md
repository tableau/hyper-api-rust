# Changelog

All notable changes to the `hyperdb-mcp` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Single-instance `hyperd` daemon** — by default, all MCP clients now
  share one `hyperd` process per user instead of each spawning their own.
  Multiple AI clients (Claude Code, Cursor, VS Code Copilot, etc.) can
  access the same persistent databases simultaneously with reduced
  resource overhead. The daemon auto-spawns on first client connect and
  shuts down after 30 minutes idle. Pass `--no-daemon` to opt out.
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
- **Catalog updates are database-aware.** `_table_catalog` rows are
  only written when ingesting to the primary or persistent database.
  User-attached databases manage their own metadata if any.

### Limitations (deferred to a follow-up)

- `load_files` and `watch_directory` reject `database` / `persist`
  with a clear error — their connection pool is bound to the primary
  database and can't reach attached databases. Use `load_file` with
  `persist: true` for one-off persistent ingests until pool routing
  ships.
- `load_file` with `mode: "merge"` rejects non-primary `database`.
  The merge implementation uses a temp table and cross-database DML
  isn't yet verified across all hyperd versions.

### Removed

- **`--bare` flag.** Catalog seeding is now uniform: created when MCP
  creates a fresh `.hyper`, never touched on existing files. Users
  who want a pristine `.hyper` for export can `DROP TABLE _table_catalog`
  once after creation; subsequent opens won't recreate it.

### Performance

- **Per-engine `_table_catalog` presence cache.** Catalog reads/writes
  used to round-trip a `pg_catalog.pg_tables` probe on every call;
  now the existence check is cached for the engine's lifetime and
  primed immediately after `CREATE TABLE IF NOT EXISTS`.

### Fixed

- **Watcher recovery after hyperd restart.** The watcher's connection
  pool now auto-rebuilds when a per-file ingest hits a connection-lost
  error (typically after the daemon restarts hyperd). Each ingest gets
  one retry on a fresh pool; persistent failures still flow into the
  standard `failed/` move so a single broken file can't pin the
  watcher in retry loops.

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
