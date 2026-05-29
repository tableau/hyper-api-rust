# Changelog

## [0.3.0](https://github.com/tableau/hyper-api-rust/compare/v0.2.3...v0.3.0) (2026-05-29)

This release aggregates a coordinated set of breaking and additive API changes that landed across four PRs during the v0.3.0 bundle window. See [MIGRATING-0.3.md](./MIGRATING-0.3.md) for complete migration recipes covering every change.

### ⚠ BREAKING CHANGES

* **Flat `Error` enum.** The public `hyperdb_api::Error` is now a flat canonical structure per the [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/) — no `Box<dyn StdError>` cause channel, no `kind()` method, no `Other` catch-all variant. `Error::new` and `Error::with_cause` are deleted in favor of domain-specific snake_case constructors (`Error::connection`, `Error::server`, `Error::conversion`, etc.). The `ErrorKind` re-export from `hyperdb_api` is removed. ([#70](https://github.com/tableau/hyper-api-rust/issues/70), [#71](https://github.com/tableau/hyper-api-rust/pull/71))
* **Transaction API consolidation.** `Connection::begin_transaction` / `commit` / `rollback` (and the async equivalents) are deprecated and `#[doc(hidden)]`. Use the RAII guard at `Connection::transaction()` / `AsyncConnection::transaction()` instead. ([#69](https://github.com/tableau/hyper-api-rust/issues/69), [#73](https://github.com/tableau/hyper-api-rust/pull/73))
* **`FromRow` modernization.** `FromRow::from_row(&Row)` becomes `FromRow::from_row(RowAccessor<'_>)`. The blanket 1/2/3/4-tuple `FromRow` impls are deleted — define a struct with `#[derive(FromRow)]` instead. New `RowAccessor` carries a per-query cached column-name → index lookup; new `Row::get_by_name` for one-off named access. ([#61](https://github.com/tableau/hyper-api-rust/issues/61), [#62](https://github.com/tableau/hyper-api-rust/issues/62), [#74](https://github.com/tableau/hyper-api-rust/pull/74))
* **Structured SQLSTATE on `Cancelled` / `Closed` / `Connection`.** `Error::Cancelled` and `Error::Closed` change from tuple to struct variants carrying `sqlstate: Option<String>`. `Error::Connection` gains the same field. `Error::sqlstate()` now returns `Some(...)` for these variants when the server provided a code (previously Server-only). New `Error::InvalidOperation` variant separates caller-API misuse from library invariant violations. ([#76](https://github.com/tableau/hyper-api-rust/pull/76))

### Features

* `#[derive(FromRow)]` proc-macro with `#[hyperdb(rename = "...")]` and `#[hyperdb(index = N)]` attributes, lives in the new re-exported `hyperdb-api-derive` crate ([#74](https://github.com/tableau/hyper-api-rust/pull/74))
* `RowAccessor` accessors: `get` / `get_opt` (name-based) and `position` / `position_opt` (index-based) ([#74](https://github.com/tableau/hyper-api-rust/pull/74))
* Ergonomic snake_case constructors workspace-wide for every error variant — `&str`, `String`, `format!(...)` accepted without `.to_string()` ceremony ([#71](https://github.com/tableau/hyper-api-rust/pull/71))
* Typed `io::Error` sources preserved on `HyperProcess` lifecycle errors ([#76](https://github.com/tableau/hyper-api-rust/pull/76))
* stabilize v0.3.0 public API bundle ([#77](https://github.com/tableau/hyper-api-rust/issues/77)) ([ac39b2c](https://github.com/tableau/hyper-api-rust/commit/ac39b2cc0ef77ecfbe3abcff965c985635e10fdf))

### Deferred

* Internal `client::Error` flatten — deferred to v0.3.x as [#75](https://github.com/tableau/hyper-api-rust/issues/75) (internal type, zero external consumers; scope grew on second look).
## [0.2.3](https://github.com/tableau/hyper-api-rust/compare/v0.2.2...v0.2.3) (2026-05-27)


### Bug Fixes

* **ci:** use exact-name match for required check-runs (no regex) ([#54](https://github.com/tableau/hyper-api-rust/issues/54)) ([fc13637](https://github.com/tableau/hyper-api-rust/commit/fc13637b0da39e98f0dc3da3034b23014ba6dc33))

## [0.2.2](https://github.com/tableau/hyper-api-rust/compare/v0.2.1...v0.2.2) (2026-05-27)


### Bug Fixes

* **ci:** defer fromJson(release.outputs.pr) into a run block ([#51](https://github.com/tableau/hyper-api-rust/issues/51)) ([dd78df9](https://github.com/tableau/hyper-api-rust/commit/dd78df978eaf244617e83ba2d8d71b680ad52876))
* clean version stamps on release builds (no -dirty markers) ([#50](https://github.com/tableau/hyper-api-rust/issues/50)) ([5962a4e](https://github.com/tableau/hyper-api-rust/commit/5962a4e3df3ff16ac29cb660d96f22907b9374a5))

## [0.2.1](https://github.com/tableau/hyper-api-rust/compare/v0.2.0...v0.2.1) (2026-05-26)


### Bug Fixes

* **build:** add make targets for API-only build and test ([#44](https://github.com/tableau/hyper-api-rust/issues/44)) ([7f81ead](https://github.com/tableau/hyper-api-rust/commit/7f81eadd690bdd09fe04a9ec2f819fbc0e041004))

## [0.2.0](https://github.com/tableau/hyper-api-rust/compare/v0.1.3...v0.2.0) (2026-05-26)


### Features

* **mcp:** ephemeral-primary + persistent-attached two-database model ([#29](https://github.com/tableau/hyper-api-rust/issues/29)) ([025ffa7](https://github.com/tableau/hyper-api-rust/commit/025ffa71bd894fa1763e89b7399e4e97e6ac6d25))
* **mcp:** finish persistent — remove all v1 limitations + per-database catalog ([#32](https://github.com/tableau/hyper-api-rust/issues/32)) ([b420532](https://github.com/tableau/hyper-api-rust/commit/b42053253a282a93e128c7035f4d25b0bc8971b3))
* **mcp:** per-tool database parameter and persist shorthand ([#31](https://github.com/tableau/hyper-api-rust/issues/31)) ([37336c8](https://github.com/tableau/hyper-api-rust/commit/37336c8791f8cdde1a14054636a09676527944fc))
* single-instance daemon for shared hyperd across MCP clients ([#26](https://github.com/tableau/hyper-api-rust/issues/26)) ([e2c6204](https://github.com/tableau/hyper-api-rust/commit/e2c6204ee22970d853d478e7679b6963e31bbc66))


### Bug Fixes

* chart time-axis rendering, auto-detection, and MCP ergonomic fixes ([#39](https://github.com/tableau/hyper-api-rust/issues/39)) ([e6d14d3](https://github.com/tableau/hyper-api-rust/commit/e6d14d33db02a26500b79ab207bd871a471ef4fa))
* **ci:** add release-please version markers to hyperdb-mcp ([#41](https://github.com/tableau/hyper-api-rust/issues/41)) ([f566bc7](https://github.com/tableau/hyper-api-rust/commit/f566bc7a73d9dfc438f427026c785a9684072ddd))
* **ci:** add release-please version markers to hyperdb-mcp dependency ([f566bc7](https://github.com/tableau/hyper-api-rust/commit/f566bc7a73d9dfc438f427026c785a9684072ddd))
* **ci:** resolve daemon test interference on macOS/Windows and disable release-please ([#28](https://github.com/tableau/hyper-api-rust/issues/28)) ([51fc9fe](https://github.com/tableau/hyper-api-rust/commit/51fc9fed17cdc6835dd15be7c1122a38aa422cdc))
* **mcp:** cross-process catalog write safety via optimistic concurrency ([#38](https://github.com/tableau/hyper-api-rust/issues/38)) ([54e3f18](https://github.com/tableau/hyper-api-rust/commit/54e3f18ebc4d79eb09df4d0663011ae49013ca17))
* **mcp:** finish-persistent follow-ups — alias canonicalization, execute reconcile, e2e harness ([#33](https://github.com/tableau/hyper-api-rust/issues/33)) ([242be20](https://github.com/tableau/hyper-api-rust/commit/242be20680411d89ace701bf44b9c090a0c8f4c8))
* **tests:** relax timing assertion and increase daemon startup timeout ([#30](https://github.com/tableau/hyper-api-rust/issues/30)) ([56a19d1](https://github.com/tableau/hyper-api-rust/commit/56a19d126212fe3b53adfb3d7770b9cfce451b37))

## [0.1.3](https://github.com/tableau/hyper-api-rust/compare/v0.1.2...v0.1.3) (2026-05-18)


### Bug Fixes

* v0.1.2 release — bump versions and add safety net ([#17](https://github.com/tableau/hyper-api-rust/issues/17)) ([bae4536](https://github.com/tableau/hyper-api-rust/commit/bae453600ce94ddc318ccb1cfe89be8fa32eef85))

## [0.1.2](https://github.com/tableau/hyper-api-rust/compare/v0.1.1...v0.1.2) (2026-05-18)


### Bug Fixes

* **ci:** include README.md in hyperdb-mcp npm package ([c8ccc22](https://github.com/tableau/hyper-api-rust/commit/c8ccc226a1540130e2e1ee6b0036fb4ccc668c4c))
* **ci:** include README.md in hyperdb-mcp npm package ([#12](https://github.com/tableau/hyper-api-rust/issues/12)) ([b1ddb33](https://github.com/tableau/hyper-api-rust/commit/b1ddb337ed8c197fb346f2b4a809f8980166e82c))
* **ci:** prevent npm-publish chmod step from failing on missing binaries ([2708ee4](https://github.com/tableau/hyper-api-rust/commit/2708ee46a51f38cbe432d629736578da1e5d2e42))
* **ci:** prevent npm-publish chmod step from failing on missing binaries ([#11](https://github.com/tableau/hyper-api-rust/issues/11)) ([bc9bee5](https://github.com/tableau/hyper-api-rust/commit/bc9bee50b9b9fbc574eb2201f7559a76248a80c9))
* **ci:** remove brew rust on macOS before installing toolchain ([b331607](https://github.com/tableau/hyper-api-rust/commit/b331607e73f185a2c301499190ccd739d0b52a7d))
* **ci:** remove brew-rust uninstall steps that delete cargo/rustc on new image ([af798f1](https://github.com/tableau/hyper-api-rust/commit/af798f16782fd45b2891e53a210ba55db9429f92))
* **ci:** restructure release-please config for workspace version inheritance ([d5ad018](https://github.com/tableau/hyper-api-rust/commit/d5ad01884e81acec9f1cebb263d72de3a7c4c418))
* **ci:** restructure release-please config for workspace version inheritance ([#13](https://github.com/tableau/hyper-api-rust/issues/13)) ([fd18a8b](https://github.com/tableau/hyper-api-rust/commit/fd18a8bde3843e0162c57d361b8b1e2b19d61d6e))
* **ci:** use simple release-type to avoid Cargo workspace member walking ([3884162](https://github.com/tableau/hyper-api-rust/commit/3884162aec551894de0b697816b34f87034ad781))
* **ci:** use simple release-type to avoid Cargo workspace member walking ([#14](https://github.com/tableau/hyper-api-rust/issues/14)) ([42f0524](https://github.com/tableau/hyper-api-rust/commit/42f0524bccf9ceaede166742c04aacc5f426f4d6))
