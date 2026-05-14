# Changelog

All notable changes to the `hyperdb-mcp` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
