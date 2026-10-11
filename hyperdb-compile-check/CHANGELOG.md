# Changelog

All notable changes to the `hyperdb-compile-check` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

This crate is the compile-time SQL validation engine behind
`hyperdb-api-derive`'s `compile-time` feature. It is published because Cargo
requires it — `hyperdb-api-derive` depends on it — but it is not a public API
surface, and consumers should depend on `hyperdb-api-derive` instead.

## [Unreleased]

### Fixed

- **SQL containing `$N` placeholders is now validated.** The dry-run executed
  the statement with nothing bound, so Hyper rejected every parameterized
  query with "expected to have 0 parameter(s)". Such queries are now
  prepared with one unspecified-type parameter per placeholder.

## [1.0.0-rc.3] - 2026-09-07

### Fixed

- **Validation no longer reports false "not registered" errors when nothing has
  been registered in the current process.** Registration is a process-global
  side effect of expanding `derive(Table)`. Under `cargo` that ordering is
  guaranteed — rustc expands a crate's macros in one host process, and
  struct-level derives expand before function-body macros. rust-analyzer's
  `proc-macro-srv` is long-lived and expands lazily, out of order, and from
  cache, so a `query_as!` could be re-expanded in a process where no derive had
  run. The result was a red squiggle in the editor on code that `cargo check`
  compiled cleanly.

  An empty registry is now treated as "no information available" and validation
  is skipped. Genuine diagnostics are unaffected: once anything is registered,
  a lookup miss is still a real miss.

### Changed

- The minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`.
