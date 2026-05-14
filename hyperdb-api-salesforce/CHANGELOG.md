# Changelog

All notable changes to the `hyperdb-api-salesforce` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
