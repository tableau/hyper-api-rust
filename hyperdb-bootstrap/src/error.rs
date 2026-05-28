// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Error types returned by the `hyperd-bootstrap` crate.

use thiserror::Error;

/// Errors produced while downloading, verifying, and installing `hyperd`.
///
/// Every fallible function in this crate returns a `Result<T, Error>`. The
/// variants line up with the phases of bootstrap: platform detection,
/// HTTP/curl fetching, TOML parsing, archive extraction, and checksum
/// verification.
#[derive(Debug, Error)]
pub enum Error {
    /// The host (`os` / `arch` combination) has no published `hyperd` build.
    #[error("unsupported platform: os={os} arch={arch}")]
    UnsupportedPlatform {
        /// Operating-system identifier returned by `std::env::consts::OS`.
        os: String,
        /// Architecture identifier returned by `std::env::consts::ARCH`.
        arch: String,
    },

    /// A platform slug (e.g. `"macos-arm64"`) did not match any known target.
    #[error("unknown platform slug: {0}")]
    UnknownPlatformSlug(String),

    /// A filesystem or I/O operation failed, enriched with contextual text.
    #[error("{context}: {source}")]
    Io {
        /// Human-readable description of the operation that was attempted.
        context: String,
        /// Underlying `std::io::Error` returned by the OS.
        #[source]
        source: std::io::Error,
    },

    /// A `reqwest` HTTP client error (connection failure, TLS issue, etc.).
    #[error("HTTP error: {0}")]
    Http(#[source] reqwest::Error),

    /// A server returned a non-success HTTP status while fetching `url`.
    #[error("HTTP {status} when fetching {url}")]
    HttpStatus {
        /// URL that was being fetched when the failure occurred.
        url: String,
        /// HTTP response status code.
        status: u16,
    },

    /// The fallback `curl` subprocess exited with a non-zero status.
    #[error("curl exited with code {code} when fetching {url}")]
    CurlFailed {
        /// URL passed to `curl`.
        url: String,
        /// `curl` exit code.
        code: i32,
    },

    /// The downloaded archive did not match the expected SHA-256 checksum.
    #[error("sha256 mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// Hex-encoded expected digest (from `hyperd-version.toml`).
        expected: String,
        /// Hex-encoded digest computed from the downloaded bytes.
        actual: String,
    },

    /// The `hyperd-version.toml` file could not be parsed.
    #[error("failed to parse version TOML: {0}")]
    TomlParse(#[source] toml::de::Error),

    /// The downloaded ZIP archive was malformed or could not be extracted.
    #[error("zip error: {0}")]
    Zip(#[source] zip::result::ZipError),

    /// The archive did not contain a recognizable `hyperd` executable.
    #[error("hyperd executable not found in extracted archive")]
    HyperdNotInArchive,

    /// Scraping the public releases page for the latest version failed.
    #[error("failed to scrape latest release: {0}")]
    ScrapeFailed(&'static str),
}

impl Error {
    /// Constructs an [`Self::UnsupportedPlatform`] error.
    pub fn unsupported_platform(os: impl Into<String>, arch: impl Into<String>) -> Self {
        Error::UnsupportedPlatform {
            os: os.into(),
            arch: arch.into(),
        }
    }

    /// Constructs an [`Self::UnknownPlatformSlug`] error.
    pub fn unknown_platform_slug(slug: impl Into<String>) -> Self {
        Error::UnknownPlatformSlug(slug.into())
    }

    /// Constructs an [`Self::Io`] error.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// Constructs an [`Self::HttpStatus`] error.
    pub fn http_status(url: impl Into<String>, status: u16) -> Self {
        Error::HttpStatus {
            url: url.into(),
            status,
        }
    }

    /// Constructs an [`Self::CurlFailed`] error.
    pub fn curl_failed(url: impl Into<String>, code: i32) -> Self {
        Error::CurlFailed {
            url: url.into(),
            code,
        }
    }

    /// Constructs an [`Self::ChecksumMismatch`] error.
    pub fn checksum_mismatch(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Error::ChecksumMismatch {
            expected: expected.into(),
            actual: actual.into(),
        }
    }
}
