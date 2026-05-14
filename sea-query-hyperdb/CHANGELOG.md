# Changelog

All notable changes to the `sea-query-hyperdb` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.1] - 2026-05-13

### Added

- `HyperQueryBuilder` implementing `sea_query::QueryBuilder` for HyperDB SQL dialect
- PostgreSQL-compatible SQL generation with Hyper-specific type handling
- Support for all standard sea-query operations (SELECT, INSERT, UPDATE, DELETE, CREATE TABLE)
