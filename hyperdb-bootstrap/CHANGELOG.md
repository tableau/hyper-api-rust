# Changelog

All notable changes to the `hyperdb-bootstrap` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

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
