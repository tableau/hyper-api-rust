# Changelog

## [1.0.0-rc.2](https://github.com/tableau/hyper-api-rust/compare/v1.0.0-rc.1...v1.0.0-rc.2) (2026-09-06)


### ⚠ BREAKING CHANGES

* **mcp:** `hyperdb-mcp` is published to crates.io, and this reshapes its library surface: `Engine::execute_in_transaction` takes `&mut self` and yields `EngineTransaction` rather than `&Engine`, the seven ingest entry points and `merge_via_temp_table` take `&mut Engine`, and `Engine::with_search_path` is new. The `missing_docs` allow reason on `src/lib.rs` claimed the crate is not published; corrected in passing.
* **core:** bind scaled Numeric and Geography as text parameters ([#257](https://github.com/tableau/hyper-api-rust/issues/257))
* **bootstrap:** source hyperd from the PyPI tableauhyperapi wheels ([#254](https://github.com/tableau/hyper-api-rust/issues/254))
* **bootstrap:** hyperdb-bootstrap no longer has a build id or a releases-page scraper, because a wheel URL is fully constructible from the version alone. Removed public API:
    - `PinnedRelease::build_id` and `InstalledHyperd::build_id` — use `.version`,
      now the only release identifier, plus the new
      `PinnedRelease::wheel_tag_for(Platform)` for the per-platform wheel tag.
    - `PinnedRelease::version_tag()` — use `.version`. It existed only to join the
      version and the build id into `0.0.26479.r96880f6a`; there is no build id
      left to join.
    - `VersionSource::ScrapeLatest` and the `scrape` module — deleted with no
      replacement. With a constructible URL and PyPI-published digests there is
      nothing left to discover.
    - `Error::Http`, `Error::HttpStatus` and `Error::ScrapeFailed` — all three
      existed only to serve the scraper. `Error::MissingWheelTag` is new, because
      `url::build_download_url` is now fallible.
    - the `--latest` CLI flag — no replacement, it is deleted along with the
      scraper it drove. Pass `--version X` or `--version-file PATH` instead.
    - the `--build-id` CLI flag — it simply goes away. Wheel URLs need no build
      id, so `--version X` on its own is now a complete version source: it
      inherits the builtin pin's `[wheel_tag]` values and carries no digests, so
      the download is unverified and logs a WARN. Use `--version-file` with a full
      pin when you need verified bytes.
    - the `regex`, `reqwest` and `rustls` dependencies — no in-process HTTP client
      remains, which also retires the rustls crypto-provider workaround.
* **grpc:** report Arrow failures in label lookups instead of a partial map

### Features

* **api:** constraint-preserving table copy for hyper export ([#258](https://github.com/tableau/hyper-api-rust/issues/258)) ([4eb61d5](https://github.com/tableau/hyper-api-rust/commit/4eb61d56fa3c13a6abaee202d997a998fedefd9a))
* **bootstrap:** source hyperd from the PyPI tableauhyperapi wheels ([cb2b63d](https://github.com/tableau/hyper-api-rust/commit/cb2b63d8ae481596a14a6ce5131ec88305da7a43))
* **bootstrap:** source hyperd from the PyPI tableauhyperapi wheels ([#254](https://github.com/tableau/hyper-api-rust/issues/254)) ([fa35a45](https://github.com/tableau/hyper-api-rust/commit/fa35a45b5a9072e386f607cdda7527a306929292))
* **core:** bind scaled Numeric and Geography as text parameters ([#257](https://github.com/tableau/hyper-api-rust/issues/257)) ([8a443c6](https://github.com/tableau/hyper-api-rust/commit/8a443c6f95b15fb92bdd2c89d013df651a95f0db))
* **mcp:** report the hyperd connection descriptor in status ([#259](https://github.com/tableau/hyper-api-rust/issues/259)) ([5333bd0](https://github.com/tableau/hyper-api-rust/commit/5333bd08f8500b98f79d25cd6e2929e0719b2042))
* **mcp:** report the hyperd endpoint in connectable form in status ([5333bd0](https://github.com/tableau/hyper-api-rust/commit/5333bd08f8500b98f79d25cd6e2929e0719b2042))


### Bug Fixes

* **api:** add missing #[must_use] on the Windows-gated pipe_name ([56fb280](https://github.com/tableau/hyper-api-rust/commit/56fb280ae2379e5168128288d248d37e9392984e))
* **bench:** report decimal MB, not MiB under an MB label ([1684bfc](https://github.com/tableau/hyper-api-rust/commit/1684bfc21aaa3614904cb737ccbff8813b1c174c))
* **bootstrap:** bump pinned hyperd to 0.0.26479 (r96880f6a) ([8c99d20](https://github.com/tableau/hyper-api-rust/commit/8c99d20d15af016b42647e6a826ad0a49c4a9082))
* **bootstrap:** use the post-rename crate name in module docs and errors ([d7da987](https://github.com/tableau/hyper-api-rust/commit/d7da987ba4045a4a4a1965e091ce40a5810f52d3))
* **compile-check:** stop false "not registered" errors in rust-analyzer ([22b0a7a](https://github.com/tableau/hyper-api-rust/commit/22b0a7ab111f04f0d45d4ed69b5f2078e1b23f1d))
* correct the pre-rename hyperd-bootstrap name in user-facing errors ([0cc65e2](https://github.com/tableau/hyper-api-rust/commit/0cc65e2b997aafcd2a607bda022eb59fade2f58f))
* **grpc:** report Arrow failures in label lookups instead of a partial map ([1cbdd4d](https://github.com/tableau/hyper-api-rust/commit/1cbdd4d61f8f60cfb2cf9fcd7ed8f0f5024d723f))
* identify hyperd by executable path, and report decimal MB in the bench harness ([#256](https://github.com/tableau/hyper-api-rust/issues/256)) ([816ecbb](https://github.com/tableau/hyper-api-rust/commit/816ecbb5576e1ae71b28755d5a73bdc3aefc7124))
* **mcp:** identify hyperd by executable, not thread name ([14dd334](https://github.com/tableau/hyper-api-rust/commit/14dd33419692fae4f6f79d29ec761ba10d136e83))
* **mcp:** scope a Unix-only test import so Windows sees no unused import ([6d2a0b5](https://github.com/tableau/hyper-api-rust/commit/6d2a0b5dac12bb846f37a10596187d1b14a8aced))


### Miscellaneous Chores

* release 1.0.0-rc.2 ([a9fe1b0](https://github.com/tableau/hyper-api-rust/commit/a9fe1b04dd2e2c2aafb12346c0b5b41436f0d787))
* release 1.0.0-rc.2 ([099ad49](https://github.com/tableau/hyper-api-rust/commit/099ad49a5c17f1c8274e91a0821aa3657e19a0c1))


### Code Refactoring

* **mcp:** drive Engine transactions with the RAII guard, and flatten client::Error ([#261](https://github.com/tableau/hyper-api-rust/issues/261)) ([68ff688](https://github.com/tableau/hyper-api-rust/commit/68ff688ebd22b29494ff69479133a4bb35db7f63))

## [1.0.0-rc.1](https://github.com/tableau/hyper-api-rust/compare/v0.7.3...v1.0.0-rc.1) (2026-09-05)


### ⚠ BREAKING CHANGES

* uplift to Rust 1.88 / edition 2024 and add a RHEL rust-toolset gate ([#250](https://github.com/tableau/hyper-api-rust/issues/250))
* **deps:** bump arrow/parquet to 59, removing the thrift advisory
* **api:** replace the deprecated transaction methods with *_unguarded
* **deps:** drop the aws-lc-rs crypto provider in favour of ring
* **node:** reject integer narrowing instead of silently truncating
* **toolchain:** migrate the workspace to Rust edition 2024
* **toolchain:** raise MSRV floor to Rust 1.88

### Features

* **api:** replace the deprecated transaction methods with *_unguarded ([bd9dc6d](https://github.com/tableau/hyper-api-rust/commit/bd9dc6dffdce9a8b4d25d5082ffa74eb50609cf1))
* **node:** reject integer narrowing instead of silently truncating ([82db12d](https://github.com/tableau/hyper-api-rust/commit/82db12ddd2cbfd2756249a5cf532db2fe24aff28))
* **toolchain:** flatten 127 nested conditionals into let chains ([352a0c4](https://github.com/tableau/hyper-api-rust/commit/352a0c433f26d8c780a0a9cccadc1a75fc506c6a))
* **toolchain:** migrate the workspace to Rust edition 2024 ([efbf704](https://github.com/tableau/hyper-api-rust/commit/efbf7043e925565d67c0af44edca6d6b082153bc))
* **toolchain:** raise MSRV floor to Rust 1.88 ([091327c](https://github.com/tableau/hyper-api-rust/commit/091327cba856fcb35d8d52652037f4c6957c5e2c))
* uplift to Rust 1.88 / edition 2024 and add a RHEL rust-toolset gate ([#250](https://github.com/tableau/hyper-api-rust/issues/250)) ([567f819](https://github.com/tableau/hyper-api-rust/commit/567f819f95b00e933296a15fb9a1df619da4c12a))


### Bug Fixes

* **deps:** bump arrow/parquet to 59, removing the thrift advisory ([3cb4b26](https://github.com/tableau/hyper-api-rust/commit/3cb4b266b40216139506e82b7deb3b5005875c1b))
* **deps:** drop the aws-lc-rs crypto provider in favour of ring ([909127c](https://github.com/tableau/hyper-api-rust/commit/909127ce4bafe996b462fed7442c73636979d264))
* **docs:** restore README structure mangled by a markdown formatter ([2b275ea](https://github.com/tableau/hyper-api-rust/commit/2b275ea097528bbda535a8b68aba94eb955ce1ae))
* ignore nested target/ dirs and Python bytecode caches ([e87e7ec](https://github.com/tableau/hyper-api-rust/commit/e87e7ecb909dd0bd43bf9a7f5074a95c22ce30f5))
* **protocol:** remove a latent usize overflow in the variable-length readers ([d7157c3](https://github.com/tableau/hyper-api-rust/commit/d7157c3587383325af6d2e87f6b75525c3c02870))
* **release:** unblock prerelease versions in CI ([224890a](https://github.com/tableau/hyper-api-rust/commit/224890a5d5a73d6ce0b81230ee38c8568ba804ac))
* **release:** unblock prerelease versions in CI ([#252](https://github.com/tableau/hyper-api-rust/issues/252)) ([0b111b3](https://github.com/tableau/hyper-api-rust/commit/0b111b32bc4fa44a1c740931ceadd15f5998d755))
* **tls:** install the ring crypto provider before building HTTP clients ([0cb8a36](https://github.com/tableau/hyper-api-rust/commit/0cb8a36fc36bb99c4917e6acd5fa0d28d9d711d7))


### Performance Improvements

* **node:** split the columnar Int32 narrowing into two passes ([3b8a5b4](https://github.com/tableau/hyper-api-rust/commit/3b8a5b49484808f05a2cc5d9166b23922f2e0eaa))


### Miscellaneous Chores

* release 1.0.0-rc.1 ([7bf2dff](https://github.com/tableau/hyper-api-rust/commit/7bf2dff0909c4d39ec33cdf320bc190f88618ead))

## [0.7.3](https://github.com/tableau/hyper-api-rust/compare/v0.7.2...v0.7.3) (2026-08-28)

### Bug Fixes

* **mcp:** improve diagnostics, daemon resilience, and chart UX ([#243](https://github.com/tableau/hyper-api-rust/issues/243)) ([44bdf1e](https://github.com/tableau/hyper-api-rust/commit/44bdf1e5551e8936a568a1ce6ca07ac4ca5e75c7))

## [0.7.2](https://github.com/tableau/hyper-api-rust/compare/v0.7.1...v0.7.2) (2026-08-26)

### Bug Fixes

* bundle correct hyperd 0.0.26359 in npm packages ([#239](https://github.com/tableau/hyper-api-rust/issues/239)) ([59a74e0](https://github.com/tableau/hyper-api-rust/commit/59a74e0397c252d2a5a757dd48e8e2c031857d7a))

## [0.7.1](https://github.com/tableau/hyper-api-rust/compare/v0.7.0...v0.7.1) (2026-08-25)

### Bug Fixes

* **bootstrap:** bump pinned hyperd to 0.0.26359 (r07abb490) ([#237](https://github.com/tableau/hyper-api-rust/issues/237)) ([1ea0375](https://github.com/tableau/hyper-api-rust/commit/1ea037548746600ec718c51517898a5e42da6be4))
* **deps:** bump h2 for RUSTSEC-2026-0258, fix new clippy unused_async_trait_impl lint ([#235](https://github.com/tableau/hyper-api-rust/issues/235)) ([753d1f5](https://github.com/tableau/hyper-api-rust/commit/753d1f571780360111b14306abff2f2b5bccae49))

## [0.7.0](https://github.com/tableau/hyper-api-rust/compare/v0.6.1...v0.7.0) (2026-07-11)

### ⚠ BREAKING CHANGES

* **kv:** #192 KV MCP LLM ergonomics — created signal, batch, size, value_path, JSON docs ([#193](https://github.com/tableau/hyper-api-rust/issues/193))
* **kv:** async set/set_as/set_batch return SetOutcome/BatchSetOutcome
* **kv:** async twin — reshape AsyncKvStore set/set_as/set_batch
* **kv:** sync set/set_as/set_batch return SetOutcome/BatchSetOutcome
* commits the plan produces, per release-please.

### Features

* commits the plan produces, per release-please. ([f6e5789](https://github.com/tableau/hyper-api-rust/commit/f6e57895bb1b76fbe5c71a1d25a4fef1ffcaa241))
* **kv:** [#192](https://github.com/tableau/hyper-api-rust/issues/192) KV MCP LLM ergonomics — created signal, batch, size, value_path, JSON docs ([#193](https://github.com/tableau/hyper-api-rust/issues/193)) ([52a3ba0](https://github.com/tableau/hyper-api-rust/commit/52a3ba013f150e18785943a917f03eed2bad9cea))
* **kv:** add async set_if_absent/byte_size/entries/set_batch_if_absent ([a166813](https://github.com/tableau/hyper-api-rust/commit/a16681378b7e71a9ee8821aedd1f4ebdd4373160))
* **kv:** add set_if_absent and set_batch_if_absent to AsyncKvStore ([d116b12](https://github.com/tableau/hyper-api-rust/commit/d116b125a92d9819f1e07b0e4a1e094b581312bc))
* **kv:** add sync KvStore::byte_size and entries ([aea9ea5](https://github.com/tableau/hyper-api-rust/commit/aea9ea54d261e8f76368acd83c028374e8b1b6bc))
* **kv:** add sync KvStore::set_batch_if_absent atomic guard ([4a88aa3](https://github.com/tableau/hyper-api-rust/commit/4a88aa3b284c5843ec3a573417d7a35f15a93b20))
* **kv:** add sync KvStore::set_if_absent write guard ([4f1ca85](https://github.com/tableau/hyper-api-rust/commit/4f1ca85fabec62fb225a70e7a922f669d6053711))
* **kv:** async set/set_as/set_batch return SetOutcome/BatchSetOutcome ([6af0649](https://github.com/tableau/hyper-api-rust/commit/6af06495121ba78d1273d714c599ed8b9a8a7113))
* **kv:** async twin — reshape AsyncKvStore set/set_as/set_batch ([89a7767](https://github.com/tableau/hyper-api-rust/commit/89a7767b80f0420691a65dcfbe27bcf49f31dd2c))
* **kv:** sync set/set_as/set_batch return SetOutcome/BatchSetOutcome ([5aeab78](https://github.com/tableau/hyper-api-rust/commit/5aeab78c92c5b7f3897692da6ff4aea9943375fa))
* **mcp:** add kv_set_many atomic batch write tool ([34f15f7](https://github.com/tableau/hyper-api-rust/commit/34f15f7b0f067d47932ef48d3434b9f584c5d8c2))
* **mcp:** add values flag to kv_list for whole-store reads ([f0b3527](https://github.com/tableau/hyper-api-rust/commit/f0b35275a13bdb2289f58dd8f24e2321d1764e7a))
* **mcp:** kv_set reports created/value_bytes, adds overwrite guard + value_path ([fd89084](https://github.com/tableau/hyper-api-rust/commit/fd89084afdbfb3ffac41ae4ee255cc2312c9c557))
* **mcp:** kv_size reports total value bytes ([0967c05](https://github.com/tableau/hyper-api-rust/commit/0967c05eb7f27667ad9e1f792bc039ad332a8d06))

### Bug Fixes

* **mcp:** [#192](https://github.com/tableau/hyper-api-rust/issues/192) cap kv_set value_path file size before reading ([0fda8ec](https://github.com/tableau/hyper-api-rust/commit/0fda8ece5ac0e6065c2b797fd9fe26d6221279e9))
* **mcp:** preserve PermissionDenied and steer JSON errors to ::json cast ([e1acbcd](https://github.com/tableau/hyper-api-rust/commit/e1acbcde9cf09308acd7d805745877939fb2be9d))

## [0.6.1](https://github.com/tableau/hyper-api-rust/compare/v0.6.0...v0.6.1) (2026-07-10)

### Bug Fixes

* **mcp:** surface the KV store in the crates.io README and map caller-fixable identifier errors to INVALID_ARGUMENT ([#188](https://github.com/tableau/hyper-api-rust/issues/188)) ([a5993d5](https://github.com/tableau/hyper-api-rust/commit/a5993d5e9901a5bcc02bb9d7fbfe850ad36fe272))

## [0.6.0](https://github.com/tableau/hyper-api-rust/compare/v0.5.6...v0.6.0) (2026-07-10)

### Features

* **mcp:** key-value scratchpad tools + database-targeted KV API ([#185](https://github.com/tableau/hyper-api-rust/issues/185)) ([c24e291](https://github.com/tableau/hyper-api-rust/commit/c24e2918f60792e693b6b40f583562648233f7ac))

## [0.5.6](https://github.com/tableau/hyper-api-rust/compare/v0.5.5...v0.5.6) (2026-06-16)

### Bug Fixes

* **api:** parameterized FromRow methods (fetch_*_as_params / stream_as_params) ([#152](https://github.com/tableau/hyper-api-rust/issues/152)) ([c576945](https://github.com/tableau/hyper-api-rust/commit/c5769454c3f9e69d3b8f7ee3265c12353bc3bceb))
* **api:** point query_as!/query_scalar! param comments at [#137](https://github.com/tableau/hyper-api-rust/issues/137) ([#153](https://github.com/tableau/hyper-api-rust/issues/153)) ([926019d](https://github.com/tableau/hyper-api-rust/commit/926019da364c9c9c4f40334e3113021697b70985))
* **ci:** grant contents:write to release publish job ([5160975](https://github.com/tableau/hyper-api-rust/commit/5160975aa024ff35297a44148ca34eb5b0612764))

## [0.5.5](https://github.com/tableau/hyper-api-rust/compare/v0.5.4...v0.5.5) (2026-06-16)

### Bug Fixes

* **deps:** bump npm dependencies to resolve Dependabot alerts ([f37c2d7](https://github.com/tableau/hyper-api-rust/commit/f37c2d7f79ba7f0864fea0416ff45b38d00ec932))

## [0.5.4](https://github.com/tableau/hyper-api-rust/compare/v0.5.3...v0.5.4) (2026-06-10)

### Bug Fixes

* **api:** IntoValue + Inserter::add_geography for Geography ([#65](https://github.com/tableau/hyper-api-rust/issues/65)) ([c26242e](https://github.com/tableau/hyper-api-rust/commit/c26242e448055816ef5e01ef5e5a7509e06f2a77))
* **api:** ToSqlParam for Numeric (scale=0), Interval, and JSON ([#65](https://github.com/tableau/hyper-api-rust/issues/65)) ([2e4a4d7](https://github.com/tableau/hyper-api-rust/commit/2e4a4d7ddf3d037a9e8068a1c97ecc7206aea3ed))
* catalog rename metadata, data_url field, and ToSqlParam/IntoValue type coverage ([#134](https://github.com/tableau/hyper-api-rust/issues/134)) ([00fd333](https://github.com/tableau/hyper-api-rust/commit/00fd333ccc62c060707ed8d5105cdf9b7277a330))
* **mcp:** add data_url field to_table_catalog for mechanical refresh ([#60](https://github.com/tableau/hyper-api-rust/issues/60)) ([50cdb25](https://github.com/tableau/hyper-api-rust/commit/50cdb25be7c4a57fe20490eeb1c83c21295d7146))
* **mcp:** preserve catalog metadata on ALTER TABLE RENAME ([#59](https://github.com/tableau/hyper-api-rust/issues/59)) ([e979bba](https://github.com/tableau/hyper-api-rust/commit/e979bba606737c8ce46d97fa495278cd6a51ed9e))

## [0.5.3](https://github.com/tableau/hyper-api-rust/compare/v0.5.2...v0.5.3) (2026-06-09)

### Bug Fixes

* **mcp:** avoid deadlock in heartbeat by passing health port directly ([26fd046](https://github.com/tableau/hyper-api-rust/commit/26fd046fb1c6df246f5059d2242839db1d2ed951))
* **mcp:** default chart inline to true for immediate display ([8792ef3](https://github.com/tableau/hyper-api-rust/commit/8792ef37d6c0114be7c10e0a322b0614c6a64bb7))
* **mcp:** heartbeat deadlock, chart inline default, lock-free status fast path ([#118](https://github.com/tableau/hyper-api-rust/issues/118)) ([#126](https://github.com/tableau/hyper-api-rust/issues/126)) ([8115109](https://github.com/tableau/hyper-api-rust/commit/81151096391bd07c51c257e91b8486b8f9679ae5))
* **mcp:** lock-free status fast path so diagnostics never hang ([#118](https://github.com/tableau/hyper-api-rust/issues/118)) ([0f8fa5a](https://github.com/tableau/hyper-api-rust/commit/0f8fa5a4e2f56deb2ed808a20777d541c5d07b9e))
* remove redundant Duration import in connect_named_pipe ([02feadc](https://github.com/tableau/hyper-api-rust/commit/02feadc71536259cee2d26a4818bc28d8b374d91))

## [0.5.2](https://github.com/tableau/hyper-api-rust/compare/v0.5.1...v0.5.2) (2026-06-08)

### Bug Fixes

* **bootstrap:** download hyperd from the Java API bundle, not C++ ([#121](https://github.com/tableau/hyper-api-rust/issues/121)) ([9ca4f24](https://github.com/tableau/hyper-api-rust/commit/9ca4f240c17ff816f9d2cfb833a344def2c7fbfc))

## [0.5.1](https://github.com/tableau/hyper-api-rust/compare/v0.5.0...v0.5.1) (2026-06-08)

### Bug Fixes

* **client:** enable TCP keepalive on hyperd connections ([8de3b0e](https://github.com/tableau/hyper-api-rust/commit/8de3b0e8c2ec23eeae15477cf6731877dc3d074f))
* **daemon:** stop redundant off-base daemon after concurrent cold-start ([655560e](https://github.com/tableau/hyper-api-rust/commit/655560e2e561c41a6ec98a08d4c99da55ed5a717))

## [0.5.0](https://github.com/tableau/hyper-api-rust/compare/v0.4.0...v0.5.0) (2026-06-07)

### Features

* **daemon:** identified PONG handshake + port-scan resolver groundwork ([751fd75](https://github.com/tableau/hyper-api-rust/commit/751fd7576f82aa22c4ac619bcf7a17b89f904bc0))
* **daemon:** keep daemon resident by default; CLI auto-discovers port ([e123688](https://github.com/tableau/hyper-api-rust/commit/e12368808cc2c6e4f7de6a0a158c914e332299e2))
* **daemon:** port-scanning locator + newer-client version takeover ([114a155](https://github.com/tableau/hyper-api-rust/commit/114a1551b040084142540c124de27cbdb23cfd59))
* **mcp:** surface hyperd endpoint + daemon health port in status tool ([293a7a0](https://github.com/tableau/hyper-api-rust/commit/293a7a0fe860d5e3b2a69f7a52e89c2b0530374b))

### Bug Fixes

* **mcp:** harden daemon discovery — identified PONG, port scanning, version takeover, resident-by-default ([#115](https://github.com/tableau/hyper-api-rust/issues/115)) ([05019b9](https://github.com/tableau/hyper-api-rust/commit/05019b958fc12f35efe13a47931a50d66496ad80))

## [0.4.0](https://github.com/tableau/hyper-api-rust/compare/v0.3.1...v0.4.0) (2026-06-02)

### Features

* streaming FromRow mapping (stream_as) — constant-memory struct-mapped queries ([#91](https://github.com/tableau/hyper-api-rust/issues/91)) ([#94](https://github.com/tableau/hyper-api-rust/issues/94)) ([3327fc0](https://github.com/tableau/hyper-api-rust/commit/3327fc0fd8d61faaa55b8d5ae2922ff7284348c5))
* opt-in compile-time SQL validation — `query_as!` / `query_scalar!` validate SQL against `#[derive(Table)]` structs at build time, with VS Code diagnostics ([#93](https://github.com/tableau/hyper-api-rust/issues/93)) ([73f9b0f](https://github.com/tableau/hyper-api-rust/commit/73f9b0f))

### Bug Fixes

* **release:** wrap hyperdb-compile-check pin in release-please markers ([#97](https://github.com/tableau/hyper-api-rust/issues/97)) ([1f07015](https://github.com/tableau/hyper-api-rust/commit/1f07015d9c26cd85fe1f722b8ef5d7c52464cd8c))

## [0.3.1](https://github.com/tableau/hyper-api-rust/compare/v0.3.0...v0.3.1) (2026-05-29)

Patch release that fixes two related but distinct bugs surfaced by [#84](https://github.com/tableau/hyper-api-rust/issues/84) — wrong NUMERIC values in MCP query results and Node bindings.

### Bug Fixes

* **Core: `Numeric::Display` no longer drops the sign for sub-unit negatives.** Values in the open interval `(-1, 0)` previously rendered without the minus sign — `Numeric::new(-5000, 4).to_string()` returned `"0.5000"` instead of `"-0.5000"`. The Display impl now computes the sign explicitly and formats the magnitude via `unsigned_abs()`, which also removes a latent `i128::MIN` overflow panic. This silently flipped the sign of any correlation, 0-1 index, or regression residual that crossed the stringify path — including the MCP `query` tool's JSON serialization. ([#84](https://github.com/tableau/hyper-api-rust/issues/84), [#86](https://github.com/tableau/hyper-api-rust/pull/86))
* **Node bindings: `NUMERIC` columns no longer decode as garbage / NaN.** `extract_row` and the columnar fast path were calling `row.get_f64()` for `SqlType::Numeric` columns, which reinterpreted the unscaled-integer bytes as IEEE-754 doubles. Every NUMERIC cell was wrong, regardless of sign. The bindings now use schema-aware `row.get_numeric()`, which honors the column scale and dispatches on wire form. `getString` returns the exact decimal text (preserving scale and sign), `getFloat64` returns the lossy-but-correct double, `getInt32`/`getInt64` return the truncated integer, and the columnar `getFloat64Column` returns correct `f64` values. Related to [#84](https://github.com/tableau/hyper-api-rust/issues/84).
* **Node bindings: `getBigInt` now preserves precision on `NUMERIC(p, 0)` columns.** Previously `getBigInt` returned `null` for any NUMERIC cell. It now preserves the full 128-bit unscaled value for integer-shaped numerics — use it instead of `getInt64` for NUMERIC integer values above `Number.MAX_SAFE_INTEGER`. On `NUMERIC(p, scale>0)` columns it returns `null` (use `getString` for exact text or `getFloat64` for a lossy value). Related to [#84](https://github.com/tableau/hyper-api-rust/issues/84).

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
