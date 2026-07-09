// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Server version parsing and comparison.
//!
//! Provides a [`ServerVersion`] struct for parsing and comparing Hyper server
//! version strings. This enables feature detection based on server capabilities.
//!
//! # Example
//!
//! ```
//! use hyperdb_api::ServerVersion;
//!
//! let v = ServerVersion::parse("0.0.19038").unwrap();
//! assert_eq!(v.major(), 0);
//! assert_eq!(v.minor(), 0);
//! assert_eq!(v.patch(), 19038);
//! assert!(v >= ServerVersion::new(0, 0, 19000));
//! ```

use std::fmt;

/// A parsed server version with comparison support.
///
/// Hyper server versions typically follow the format `major.minor.patch`,
/// sometimes with additional build metadata (e.g., `0.0.19038.r12345`).
///
/// # Ordering
///
/// Versions are compared lexicographically by (major, minor, patch).
/// The `suffix` field is ignored for comparison and ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerVersion {
    major: u32,
    minor: u32,
    patch: u32,
    /// Optional suffix after the version numbers (e.g., "r12345", "beta1").
    suffix: Option<String>,
    /// The original version string.
    raw: String,
}

impl ServerVersion {
    /// Creates a new `ServerVersion` from components.
    #[must_use]
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        ServerVersion {
            major,
            minor,
            patch,
            suffix: None,
            raw: format!("{major}.{minor}.{patch}"),
        }
    }

    /// Parses a version string like "0.0.19038" or "1.2.3.r456".
    ///
    /// Accepts various production formats:
    /// - `"0.0.19038"` — standard dotted numeric
    /// - `"1.2.3.r456"` — dot-separated suffix
    /// - `"1.2.3-beta1"` — hyphen-separated pre-release
    /// - `"v1.2.3"` — optional `v`/`V` prefix
    /// - `"1.2"` — patch defaults to 0
    /// - `"  1.2.3  "` — leading/trailing whitespace is trimmed
    ///
    /// Returns `None` if the string doesn't contain at least two numeric
    /// components separated by a dot.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        // Strip optional 'v' or 'V' prefix
        let trimmed = trimmed
            .strip_prefix('v')
            .or_else(|| trimmed.strip_prefix('V'))
            .unwrap_or(trimmed);

        let mut parts = trimmed.splitn(4, '.');

        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next()?.parse().ok()?;

        // Patch is optional (default 0)
        let (patch, suffix) = if let Some(patch_str) = parts.next() {
            // The patch component may contain non-numeric trailing text,
            // e.g. "3-beta1" or "19038rc1". Extract leading digits as
            // the patch number and treat the rest as suffix.
            let num_end = patch_str
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(patch_str.len());
            if num_end == 0 {
                // No leading digits at all (e.g. "abc") — not a valid patch
                return None;
            }
            let patch: u32 = patch_str[..num_end].parse().ok()?;
            // Build suffix from any trailing text in the patch component
            // plus any remaining dot-separated part.
            let patch_tail = &patch_str[num_end..];
            let dot_tail = parts.next();
            let suffix = match (patch_tail.is_empty(), dot_tail) {
                (true, None) => None,
                (true, Some(t)) => Some(t.to_string()),
                (false, None) => Some(patch_tail.to_string()),
                (false, Some(t)) => Some(format!("{patch_tail}.{t}")),
            };
            (patch, suffix)
        } else {
            (0, None)
        };

        Some(ServerVersion {
            major,
            minor,
            patch,
            suffix,
            raw: s.trim().to_string(),
        })
    }

    /// Returns the major version number.
    #[must_use]
    pub fn major(&self) -> u32 {
        self.major
    }

    /// Returns the minor version number.
    #[must_use]
    pub fn minor(&self) -> u32 {
        self.minor
    }

    /// Returns the patch version number.
    #[must_use]
    pub fn patch(&self) -> u32 {
        self.patch
    }

    /// Returns the optional suffix (e.g., "r12345").
    #[must_use]
    pub fn suffix(&self) -> Option<&str> {
        self.suffix.as_deref()
    }

    /// Returns the original version string.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl PartialOrd for ServerVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ServerVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl fmt::Display for ServerVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic() {
        let v = ServerVersion::parse("0.0.19038").unwrap();
        assert_eq!(v.major(), 0);
        assert_eq!(v.minor(), 0);
        assert_eq!(v.patch(), 19038);
        assert_eq!(v.suffix(), None);
    }

    #[test]
    fn test_parse_with_suffix() {
        let v = ServerVersion::parse("1.2.3.r456").unwrap();
        assert_eq!(v.major(), 1);
        assert_eq!(v.minor(), 2);
        assert_eq!(v.patch(), 3);
        assert_eq!(v.suffix(), Some("r456"));
    }

    #[test]
    fn test_parse_two_parts() {
        let v = ServerVersion::parse("1.2").unwrap();
        assert_eq!(v.major(), 1);
        assert_eq!(v.minor(), 2);
        assert_eq!(v.patch(), 0);
    }

    #[test]
    fn test_parse_invalid() {
        assert!(ServerVersion::parse("").is_none());
        assert!(ServerVersion::parse("abc").is_none());
        assert!(ServerVersion::parse("1").is_none());
        assert!(ServerVersion::parse("1.2.abc").is_none());
        assert!(ServerVersion::parse("vabc").is_none());
        assert!(ServerVersion::parse("v").is_none());
    }

    #[test]
    fn test_parse_v_prefix() {
        let v = ServerVersion::parse("v1.2.3").unwrap();
        assert_eq!(v.major(), 1);
        assert_eq!(v.minor(), 2);
        assert_eq!(v.patch(), 3);
        assert_eq!(v.suffix(), None);
    }

    #[test]
    fn test_parse_uppercase_v_prefix() {
        let v = ServerVersion::parse("V1.2.3").unwrap();
        assert_eq!(v.major(), 1);
        assert_eq!(v.patch(), 3);
    }

    #[test]
    fn test_parse_hyphen_prerelease() {
        let v = ServerVersion::parse("1.2.3-beta1").unwrap();
        assert_eq!(v.major(), 1);
        assert_eq!(v.minor(), 2);
        assert_eq!(v.patch(), 3);
        assert_eq!(v.suffix(), Some("-beta1"));
    }

    #[test]
    fn test_parse_patch_with_rc_suffix() {
        let v = ServerVersion::parse("0.0.19038rc1").unwrap();
        assert_eq!(v.patch(), 19038);
        assert_eq!(v.suffix(), Some("rc1"));
    }

    #[test]
    fn test_parse_hyphen_prerelease_with_dot_suffix() {
        let v = ServerVersion::parse("1.2.3-beta.1").unwrap();
        assert_eq!(v.patch(), 3);
        // "-beta" from patch tail, ".1" from dot tail
        assert_eq!(v.suffix(), Some("-beta.1"));
    }

    #[test]
    fn test_parse_whitespace() {
        let v = ServerVersion::parse("  1.2.3  ").unwrap();
        assert_eq!(v.major(), 1);
        assert_eq!(v.patch(), 3);
    }

    #[test]
    fn test_comparison() {
        let v1 = ServerVersion::new(1, 0, 0);
        let v2 = ServerVersion::new(1, 0, 1);
        let v3 = ServerVersion::new(1, 1, 0);
        let v4 = ServerVersion::new(2, 0, 0);

        assert!(v1 < v2);
        assert!(v2 < v3);
        assert!(v3 < v4);
        assert_eq!(v1, ServerVersion::new(1, 0, 0));
    }

    #[test]
    fn test_comparison_ignores_suffix() {
        let v1 = ServerVersion::parse("1.2.3").unwrap();
        let v2 = ServerVersion::parse("1.2.3-beta1").unwrap();
        // PartialEq compares all fields, but Ord ignores suffix
        assert_eq!(v1.cmp(&v2), std::cmp::Ordering::Equal);
        assert!(v1 >= v2);
        assert!(v2 >= v1);
    }

    #[test]
    fn test_display() {
        let v = ServerVersion::parse("0.0.19038").unwrap();
        assert_eq!(format!("{v}"), "0.0.19038");
    }

    #[test]
    fn test_display_preserves_original() {
        // v-prefix is preserved in raw/display
        let v = ServerVersion::parse("v1.2.3").unwrap();
        assert_eq!(format!("{v}"), "v1.2.3");
    }
}
