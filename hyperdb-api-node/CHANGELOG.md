# Changelog

All notable changes to the `hyperdb-api-node` package will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.1] - 2026-05-13

### Added

- `HyperProcess` for managing local `hyperd` server instances
- `Connection` with async `executeQuery`, `executeCommand`, `querySchema`
- `Catalog` for schema and table introspection
- `Inserter` for bulk data insertion via HyperBinary COPY
- `ConnectionPool` (in `pool.mjs`) for async connection pooling
- Arrow IPC support via `executeQueryToArrow` and Apache Arrow integration
- Query statistics collection via `enableQueryStats` and `lastQueryStats`
- Row-level data access with typed getters and JSON serialization
- Cross-platform native binaries (macOS ARM64/x64, Linux x64/ARM64/musl, Windows x64)
