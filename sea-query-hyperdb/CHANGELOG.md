# Changelog

All notable changes to the `sea-query-hyperdb` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.0.0-rc.3] - 2026-09-07

These changes first shipped in 1.0.0-rc.1.

### Changed

- The minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`. The previous 1.81 was
  not achievable in practice — the lockfile already required 1.88 for several
  direct dependencies.

## [0.1.1] - 2026-05-13

### Added

- `HyperQueryBuilder` implementing `sea_query::QueryBuilder` for HyperDB SQL dialect
- PostgreSQL-compatible SQL generation with Hyper-specific type handling
- Support for all standard sea-query operations (SELECT, INSERT, UPDATE, DELETE, CREATE TABLE)
