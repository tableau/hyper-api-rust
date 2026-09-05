// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Error types returned by the `hyperdb-bootstrap` crate.

use thiserror::Error;

/// Errors produced while downloading, verifying, and installing `hyperd`.
///
/// Every fallible function in this crate returns a `Result<T, Error>`. The
/// variants line up with the phases of bootstrap: platform detection, URL
/// construction, `curl` fetching, TOML parsing, archive extraction, and
/// checksum verification.
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

    /// The pinned release carries no wheel tag for this platform, so the
    /// wheel file name cannot be constructed.
    #[error(
        "no wheel tag pinned for platform {platform}; add it to the [wheel_tag] table in hyperd-version.toml"
    )]
    MissingWheelTag {
        /// Platform whose `[wheel_tag]` entry is missing.
        platform: crate::platform::Platform,
    },

    /// The `curl` subprocess exited with a non-zero status.
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

    /// Constructs an [`Self::MissingWheelTag`] error.
    #[must_use]
    pub fn missing_wheel_tag(platform: crate::platform::Platform) -> Self {
        Error::MissingWheelTag { platform }
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
