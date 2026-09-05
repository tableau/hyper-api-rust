// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Download and install the `hyperd` executable from the PyPI
//! `tableauhyperapi` wheels. The wheel carries the same `hyperd` build as
//! Tableau's Java/C++ API bundles, but its file name is constructible from
//! the release version alone — no opaque build id, no page scraping — and
//! PyPI publishes a sha256 per file. See the `url` module for the full
//! rationale.
//!
//! The crate ships both a CLI binary (`hyperd-bootstrap`) and a small
//! library. The library is blocking (no async runtime required) and has
//! no dependency on `tokio`, so it can be called from build scripts,
//! `postinstall` hooks, or any sync Rust code.
//!
//! # Quick start
//!
//! ```no_run
//! use hyperdb_bootstrap::{install, InstallOptions};
//!
//! let installed = install(InstallOptions::default()).unwrap();
//! println!("hyperd is at {}", installed.binary_path.display());
//! ```
//!
//! See [`InstallOptions`] and [`VersionSource`] for how to override the
//! destination, pin a specific release, or load metadata from an external
//! TOML file.

/// HTTP (via `curl`) download of release archives + SHA-256 verification.
pub mod download;
/// Error types returned by the crate.
pub mod error;
/// ZIP archive extraction of the `hyperd` binary and its shared libraries.
pub mod extract;
/// High-level `install` entry point and its configuration types.
pub mod install;
/// Supported host platforms (macOS arm64/x86_64, Linux x86_64, Windows x86_64).
pub mod platform;
/// Pinned-release metadata loaded from `hyperd-version.toml`.
pub mod release;
/// URL construction for the PyPI wheel download endpoint.
pub mod url;
/// Reachability and digest checks for each platform of a pinned release.
pub mod verify;

pub use error::Error;
pub use install::{DEFAULT_DEST_ROOT, InstallOptions, InstalledHyperd, VersionSource, install};
pub use platform::Platform;
pub use release::PinnedRelease;
pub use verify::{DigestStatus, VerifyOutcome, verify_release};
