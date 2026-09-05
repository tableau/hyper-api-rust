# Changelog

All notable changes to the `hyperdb-api-derive` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- **BREAKING:** the minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`, so this makes the
  declared MSRV match the enterprise consumption path. The previous 1.81 was
  not achievable in practice: the lockfile already required 1.88 for several
  direct dependencies.

### Fixed

- `query_as!` and `query_scalar!` no longer report false "not registered"
  errors in rust-analyzer. Registration is a process-global side effect of
  expanding `derive(Table)`, which under `cargo` always happens before
  function-body macros in the same host process. rust-analyzer's
  `proc-macro-srv` is long-lived and expands lazily, out of order, and from
  cache, so a `query_as!` could be re-expanded in a process where no derive had
  run — yielding a red squiggle on code that `cargo check` compiles cleanly.
  Validation now treats a completely empty registry as "no information" and
  skips, rather than concluding the type is unregistered. Genuine diagnostics
  are unaffected: once anything is registered, a miss is still a real miss.

- Intra-doc links on `query_as!` and `query_scalar!` now resolve. They
  referenced `hyperdb_api` types, which this crate deliberately does not depend
  on in order to break the `hyperdb-api` -> derive -> `hyperdb-compile-check`
  dependency cycle, so they now carry explicit link-reference definitions
  pointing at docs.rs — matching the pattern already used in the crate-level
  documentation.
