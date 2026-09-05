# Changelog

All notable changes to the `hyperdb-bootstrap` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- **BREAKING:** the minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`.
- **BREAKING:** the TLS crypto provider is now **ring** rather than AWS-LC.
  This crate declares its own `reqwest` dependency rather than inheriting the
  workspace entry, and asked for the `rustls` feature — which forces the
  `aws-lc-rs` provider and, through Cargo's feature unification, imposed it on
  every crate in the workspace. It now uses `rustls-no-provider`.

  That feature links *no* provider at all, and `reqwest` resolves one through
  `CryptoProvider::get_default()`, which has no crate-feature fallback — so
  `scrape_latest` now installs ring as the process-wide default before building
  its HTTP client. **Embedders take note:** if your application installs its own
  `CryptoProvider`, install it before calling into this crate; ours defers to an
  already-installed provider rather than replacing it.

  Guarded by a unit test that builds a `reqwest` client, which needs no network
  because the failure is a panic inside `build()`.

- **Bump the pinned `hyperd` release to `0.0.26359` (`r07abb490`).** This
  supersedes the never-shipped `0.0.26225` bump attempt (PR #219), which was
  held because `0.0.26225` deadlocked on Apple Silicon Macs running macOS
  versions prior to 26 during JIT exception-frame registration. `0.0.26359`
  fixes that engine defect ("Fixed an issue in Hyper on Apple Silicon Macs
  running macOS versions prior to version 26"). Updates the version, build id,
  and all four per-platform sha256s in `hyperd-version.toml`. Performance
  (same-session A/B vs the live `0.0.25080` pin, median of 3 runs at 100M rows,
  single-connection): `full_scan` queries **+67% sync / +33% async** on the
  dominant scan path, filtered queries ~flat; bulk inserts show a small
  regression (Inserter −11%, ChunkSender −6%, async −4%) — soft numbers, as the
  insert path carries cold-start variance and multi-connection deltas throttle
  thermally on a laptop. Verified native arm64 and all 1485 workspace tests pass;
  the macOS-14 CI runner is green (the deadlock is gone).

- **Bump the pinned `hyperd` release to `0.0.26479` (`r96880f6a`).** Updates the
  version, build id, and all four per-platform sha256s in `hyperd-version.toml`.
  **Verified native arm64** — `file` reports `Mach-O 64-bit executable arm64`
  for the `macos-arm64` bundle's `lib/hyper/hyperd`, so the reason this crate
  pulls the Java bundle rather than the C++ one still holds. All four platform
  URLs return HTTP 200 at the new pin, and all **1586** workspace tests pass
  against the new engine. Performance (interleaved same-session A/B vs the
  previous `0.0.26359` pin, medians of 5 runs per engine at 100M rows,
  single-connection): the async Arrow insert path **more than doubles**
  (`AsyncArrowInserter` **+127%**, 30.4 → 68.9 M rows/s), reproduced as **+75%**
  at 10M rows. Everything else is flat — sync `Inserter` +0.1%, `ChunkSender`
  +1.4%, and all four query paths within ±0.7%. The async insert gain survives
  its own wide (±25–35%) run-to-run spread because the old and new sample
  ranges do not overlap at either scale. Multi-connection (`× 4`) deltas are
  **not** reported: on this 14-core laptop they throttle thermally and spread
  17–41% run to run, enough to have read **+23% at 100M and −20% at 10M for the
  same workload in the same session**.

### Fixed

- The "no hyperd installed" error suggested `hyperd-bootstrap download`, but
  the binary is `hyperdb-bootstrap` — the suggested command did not exist.
  Doc comments across the crate carried the same pre-rename name, as did the
  `hyperdb-bootstrap/hyperd-version.toml` path in `release.rs`.

- **Download `hyperd` from the Java API bundle instead of the C++ bundle.**
  Tableau's C++ `macos-arm64` zip ships an **x86_64** `hyperd` (an upstream
  packaging defect), so on Apple Silicon the extracted `hyperd` only ran
  under Rosetta. The Java `macos-arm64` bundle carries a native arm64
  `hyperd`. The bundles share an identical URL template (only the
  `java`/`cxx` token differs) and an identical internal layout
  (`lib/hyper/hyperd`), so the switch is confined to the URL token and the
  pinned per-platform sha256s in `hyperd-version.toml`. The other three
  platforms (`macos-x86_64`, `linux-x86_64`, `windows-x86_64`) are
  unaffected in architecture but now also come from the Java bundle for
  consistency.

## [0.1.1] - 2026-05-13

### Added

- `install()` library function and `hyperdb-bootstrap` CLI binary
- `InstallOptions` for configuring an install (destination, version source, sha256 verification)
- `InstalledHyperd` describing a successful install (path, platform, release metadata)
- `VersionSource` enum (pinned compile-time metadata, custom version file, or `--latest` scrape)
- `Platform` enum and detection for macOS (arm64/x86_64), Linux (x86_64), Windows (x86_64)
- `PinnedRelease` for the pinned release metadata baked in at compile time via `hyperd-version.toml`
- `verify_release()` library API and `verify` CLI subcommand that HEADs every supported platform's URL — wired into a GitHub Actions workflow for CI
- `VerifyOutcome` describing the result of a verification
- `Error` type for structured error handling
- `--latest` best-effort scrape of the public Tableau releases page
- `--version-file` override plus auto-discovery of `./hyperd-version.toml`
- sha256 verification when hashes are available
- `build.rs` compile-time validation of `hyperd-version.toml` (shape, hex sha256s, known platform keys; empty sha256s warn)
- `DEFAULT_DEST_ROOT` constant for the default install destination
