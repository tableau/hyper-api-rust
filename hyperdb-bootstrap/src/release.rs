// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A pinned `hyperd` release descriptor loaded from `hyperd-version.toml`.
//!
//! Each `PinnedRelease` records the release `version`, the wheel platform tag
//! to request for each target, and the expected SHA-256 checksum of each
//! platform's wheel.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::Error;
use crate::platform::Platform;

const BUILTIN_TOML: &str = include_str!("../hyperd-version.toml");

/// A concrete `hyperd` release pinned to a specific version, with per-platform
/// wheel tags and optional per-platform SHA-256 checksums.
///
/// The "built-in" pin shipped with the crate lives in
/// `hyperdb-bootstrap/hyperd-version.toml` and is available via
/// [`PinnedRelease::builtin`]. Callers can override it by loading an
/// external TOML file (see [`PinnedRelease::from_toml_file`]) or by passing
/// a literal TOML string to [`PinnedRelease::from_toml_str`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedRelease {
    /// Upstream release version (for example, `"0.0.26479"`).
    pub version: String,
    /// Wheel platform tag keyed by platform (for example,
    /// `"macosx_13_0_arm64"`). Kept as pin data rather than hardcoded because
    /// the tags are not stable across releases and a wrong tag 404s silently.
    #[serde(default)]
    pub wheel_tag: HashMap<Platform, String>,
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
        Self::lookup(&self.sha256, platform)
    }

    /// Returns the wheel platform tag for `platform` (for example,
    /// `"macosx_13_0_arm64"`), or `None` if the pin does not carry one.
    ///
    /// Without a tag the wheel filename cannot be constructed, so callers
    /// treat `None` as [`Error::MissingWheelTag`] rather than guessing.
    #[must_use]
    pub fn wheel_tag_for(&self, platform: Platform) -> Option<&str> {
        Self::lookup(&self.wheel_tag, platform)
    }

    fn lookup(table: &HashMap<Platform, String>, platform: Platform) -> Option<&str> {
        table
            .get(&platform)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_parses() {
        let r = PinnedRelease::builtin();
        assert!(!r.version.is_empty());
    }

    #[test]
    fn builtin_pins_a_wheel_tag_and_digest_for_every_platform() {
        let r = PinnedRelease::builtin();
        for p in [
            Platform::MacosArm64,
            Platform::MacosX86_64,
            Platform::LinuxX86_64,
            Platform::WindowsX86_64,
        ] {
            assert!(r.wheel_tag_for(p).is_some(), "{p} has no wheel tag");
            assert!(r.sha256_for(p).is_some(), "{p} has no sha256");
        }
    }

    #[test]
    fn empty_entries_are_ignored() {
        let toml_str = r#"
version = "0.0.1"
[wheel_tag]
"macos-arm64" = ""
"linux-x86_64" = "manylinux2014_x86_64"
[sha256]
"macos-arm64" = ""
"linux-x86_64" = "abc"
"#;
        let r = PinnedRelease::from_toml_str(toml_str).unwrap();
        assert!(r.sha256_for(Platform::MacosArm64).is_none());
        assert_eq!(r.sha256_for(Platform::LinuxX86_64), Some("abc"));
        assert!(r.wheel_tag_for(Platform::MacosArm64).is_none());
        assert_eq!(
            r.wheel_tag_for(Platform::LinuxX86_64),
            Some("manylinux2014_x86_64")
        );
    }

    #[test]
    fn tables_default_to_empty_when_absent() {
        let r = PinnedRelease::from_toml_str("version = \"0.0.1\"").unwrap();
        assert!(r.wheel_tag_for(Platform::MacosArm64).is_none());
        assert!(r.sha256_for(Platform::MacosArm64).is_none());
    }
}
