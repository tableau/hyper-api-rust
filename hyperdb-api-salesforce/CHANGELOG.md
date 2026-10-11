# Changelog

All notable changes to the `hyperdb-api-salesforce` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Security

- **Secrets no longer appear in `Debug` output or debug logs.**
  `SalesforceAuthConfig` printed `client_secret`, and `OAuthTokenResponse`,
  `OAuthToken`, `DataCloudTokenResponse` and `DataCloudToken` printed their
  access-token values; all five now redact them. The token provider also no
  longer logs the raw OAuth and DC JWT response bodies at `debug` level (it
  logs their size instead).

## [1.0.0-rc.3] - 2026-09-07

These changes first shipped in 1.0.0-rc.1.

### Changed

- The `arrow` dev-dependency (used only by the example) moved from **58** to **59**, matching
  `hyperdb-api`. This does not affect consumers.

- The minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`.
- **BREAKING:** the TLS crypto provider is now **ring** rather than AWS-LC.
  This crate's `reqwest` dependency asked for the `rustls` feature, which
  expands to `__rustls-aws-lc-rs` and forces the `aws-lc-rs` provider — quietly
  overriding the workspace's deliberate `rustls = { features = ["ring"] }`
  selection, because Cargo unifies features across the graph. It now uses
  `rustls-no-provider`, which enables reqwest's rustls plumbing without picking
  a provider.

  Because that leaves no provider at all, and `reqwest` resolves one through
  `CryptoProvider::get_default()` (which has no crate-feature fallback),
  `DataCloudTokenProvider::new` now installs ring as the process-wide default
  before building its HTTP client. **Embedders take note:** if your application
  installs its own `CryptoProvider`, install it before constructing a provider
  here; ours defers to an already-installed one rather than replacing it.

  This removes `aws-lc-sys`, which compiled AWS-LC (Amazon's BoringSSL fork)
  from source and required `cmake` plus a C++ toolchain to build. Guarded by
  `tests/crypto_provider_tests.rs`, which constructs a provider and needs no
  network or credentials because the failure is a panic inside `build()`.

## [0.1.1] - 2026-05-13

### Added

- `SalesforceAuthConfig` for configuring Data Cloud OAuth credentials
- `AuthMode` enum for selecting between JWT-bearer and other authentication flows
- `DataCloudTokenProvider` with automatic token caching and refresh
- `SharedTokenProvider` for thread-safe concurrent token access (wraps `DataCloudTokenProvider` in an `Arc`)
- `SalesforceAuthError` and `SalesforceAuthResult` for structured error handling
- `DataCloudToken` and `OAuthToken` types representing the issued credentials
- RSA private key signing for JWT assertions
- Integration with `hyperdb-api-core::client::grpc` for authenticated gRPC queries
