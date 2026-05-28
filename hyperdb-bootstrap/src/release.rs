// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A pinned `hyperd` release descriptor loaded from `hyperd-version.toml`.
//!
//! Each `PinnedRelease` records a specific `version` + `build_id` pair
//! (the two components that make up a Hyper release tag) and the expected
//! SHA-256 checksums for each platform.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::platform::Platform;
use crate::Error;

const BUILTIN_TOML: &str = include_str!("../hyperd-version.toml");

/// A concrete `hyperd` release pinned to a specific version and build, with
/// optional per-platform SHA-256 checksums.
///
/// The "built-in" pin shipped with the crate lives in
/// `hyperd-bootstrap/hyperd-version.toml` and is available via
/// [`PinnedRelease::builtin`]. Callers can override it by loading an
/// external TOML file (see [`PinnedRelease::from_toml_file`]) or by passing
/// a literal TOML string to [`PinnedRelease::from_toml_str`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedRelease {
    /// Upstream release version (for example, `"0.0.24457"`).
    pub version: String,
    /// Upstream build identifier suffix (for example, `"rc36858b6"`).
    pub build_id: String,
    /// Expected SHA-256 digests keyed by platform. Empty strings are treated
    /// as "no digest" so that partially-filled tables skip verification for
    /// the missing targets instead of failing outright.
    #[serde(default)]
    pub sha256: HashMap<Platform, String>,
}

impl PinnedRelease {
    /// Returns the `PinnedRelease` baked into the crate at build time.
    ///
    /// # Panics
    ///
    /// Panics if the shipped `hyperd-version.toml` fails to parse. This is
    /// treated as a programmer error — the file is validated by the build
    /// script and release CI.
    #[must_use]
    pub fn builtin() -> Self {
        toml::from_str(BUILTIN_TOML).expect("baked-in hyperd-version.toml must parse")
    }

    /// Parses a `PinnedRelease` from an in-memory TOML string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TomlParse`] if the text is not valid TOML or the
    /// document does not match the `PinnedRelease` schema.
    pub fn from_toml_str(s: &str) -> Result<Self, Error> {
        toml::from_str(s).map_err(Error::TomlParse)
    }

    /// Loads a `PinnedRelease` from a TOML file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be read, or
    /// [`Error::TomlParse`] if the content is not a valid `PinnedRelease`.
    pub fn from_toml_file(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|source| {
            Error::io(format!("reading version file {}", path.display()), source)
        })?;
        Self::from_toml_str(&text)
    }

    /// Returns the expected SHA-256 digest for `platform`, or `None` if the
    /// release metadata does not pin a digest for that platform. Empty
    /// strings (common in pre-release metadata) are treated as absent.
    #[must_use]
    pub fn sha256_for(&self, platform: Platform) -> Option<&str> {
        self.sha256
            .get(&platform)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }

    /// Returns the full Hyper release tag — `version.build_id` — used in
    /// download URLs and install directory names.
    #[must_use]
    pub fn version_tag(&self) -> String {
        format!("{}.{}", self.version, self.build_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_parses() {
        let r = PinnedRelease::builtin();
        assert!(!r.version.is_empty());
        assert!(!r.build_id.is_empty());
    }

    #[test]
    fn empty_sha_is_ignored() {
        let toml_str = r#"
version = "0.0.1"
build_id = "rc1"
[sha256]
"macos-arm64" = ""
"linux-x86_64" = "abc"
"#;
        let r = PinnedRelease::from_toml_str(toml_str).unwrap();
        assert!(r.sha256_for(Platform::MacosArm64).is_none());
        assert_eq!(r.sha256_for(Platform::LinuxX86_64), Some("abc"));
    }

    #[test]
    fn version_tag_format() {
        let r = PinnedRelease {
            version: "0.0.24457".to_string(),
            build_id: "rc36858b6".to_string(),
            sha256: HashMap::new(),
        };
        assert_eq!(r.version_tag(), "0.0.24457.rc36858b6");
    }
}
