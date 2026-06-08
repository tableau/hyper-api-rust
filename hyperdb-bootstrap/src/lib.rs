// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Download and install the `hyperd` executable from Tableau's Hyper Java
//! API release packages. (The Java bundle is used rather than the C++ one
//! because the C++ `macos-arm64` zip ships an x86_64 `hyperd`; see the
//! `url` module for the full rationale.)
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
//! destination, pin a specific release, load metadata from an external
//! TOML file, or scrape the latest release from the public releases page.

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
/// Best-effort scraping of the public releases page to discover the latest
/// version when no pin is supplied.
pub mod scrape;
/// URL construction for Tableau's public download endpoint.
pub mod url;
/// Reachability probes that HEAD each platform URL of a pinned release.
pub mod verify;

pub use error::Error;
pub use install::{install, InstallOptions, InstalledHyperd, VersionSource, DEFAULT_DEST_ROOT};
pub use platform::Platform;
pub use release::PinnedRelease;
pub use verify::{verify_release, VerifyOutcome};
