# Changelog

All notable changes to the `hyperdb-bootstrap` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
