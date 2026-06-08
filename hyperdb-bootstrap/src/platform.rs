// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host-platform detection and slug encoding for the four targets that
//! Tableau's Hyper API ships binaries for.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::Error;

/// One of the four platforms that Tableau publishes a `hyperd` build for.
///
/// The variant names encode both OS and architecture and map 1:1 to the
/// slugs used in the release URL structure and in `hyperd-version.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    /// Apple Silicon macOS (`aarch64-apple-darwin`).
    #[serde(rename = "macos-arm64")]
    MacosArm64,
    /// Intel macOS (`x86_64-apple-darwin`).
    #[serde(rename = "macos-x86_64")]
    MacosX86_64,
    /// 64-bit Linux (`x86_64-unknown-linux-gnu`).
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    /// 64-bit Windows (`x86_64-pc-windows-msvc`).
    #[serde(rename = "windows-x86_64")]
    WindowsX86_64,
}

impl Platform {
    /// Detects the current host's platform.
    ///
    /// Uses `std::env::consts::OS` and `std::env::consts::ARCH`. Returns
    /// [`Error::UnsupportedPlatform`] for any combination that has no
    /// published `hyperd` build.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedPlatform`] when the host OS/arch pair
    /// does not match one of the four supported targets.
    pub fn current() -> Result<Self, Error> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        match (os, arch) {
            ("macos", "aarch64") => Ok(Self::MacosArm64),
            ("macos", "x86_64") => Ok(Self::MacosX86_64),
            ("linux", "x86_64") => Ok(Self::LinuxX86_64),
            ("windows", "x86_64") => Ok(Self::WindowsX86_64),
            _ => Err(Error::unsupported_platform(os, arch)),
        }
    }

    /// Returns the kebab-case slug used in release URLs and version metadata
    /// (for example, `"macos-arm64"`).
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::MacosArm64 => "macos-arm64",
            Self::MacosX86_64 => "macos-x86_64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::WindowsX86_64 => "windows-x86_64",
        }
    }

    /// Returns the file name of the `hyperd` executable on this platform
    /// (`"hyperd.exe"` on Windows, `"hyperd"` elsewhere).
    #[must_use]
    pub fn executable_name(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "hyperd.exe",
            _ => "hyperd",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

impl std::str::FromStr for Platform {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "macos-arm64" => Ok(Self::MacosArm64),
            "macos-x86_64" => Ok(Self::MacosX86_64),
            "linux-x86_64" => Ok(Self::LinuxX86_64),
            "windows-x86_64" => Ok(Self::WindowsX86_64),
            other => Err(Error::unknown_platform_slug(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_roundtrip() {
        for p in [
            Platform::MacosArm64,
            Platform::MacosX86_64,
            Platform::LinuxX86_64,
            Platform::WindowsX86_64,
        ] {
            assert_eq!(p.slug().parse::<Platform>().unwrap(), p);
        }
    }

    #[test]
    fn executable_name_windows() {
        assert_eq!(Platform::WindowsX86_64.executable_name(), "hyperd.exe");
        assert_eq!(Platform::LinuxX86_64.executable_name(), "hyperd");
    }
}
