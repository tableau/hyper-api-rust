// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Builds canonical download URLs for Tableau's public Hyper **Java** API
//! release bundles.
//!
//! We deliberately use the Java binding's bundle rather than the C++ one:
//! the C++ `macos-arm64` zip ships an **x86_64** `hyperd` (an upstream
//! packaging defect), so on Apple Silicon it would only run under Rosetta.
//! The Java `macos-arm64` bundle carries a native arm64 `hyperd`. Both
//! bundles share the identical URL template (only the `java`/`cxx` token
//! differs) and the identical internal layout (`lib/hyper/hyperd`), so the
//! switch is confined to this token and the pinned sha256s.

use crate::platform::Platform;
use crate::release::PinnedRelease;

const BASE_URL: &str = "https://downloads.tableau.com/tssoftware";

/// Builds the `downloads.tableau.com` URL for the given release / platform
/// combination.
///
/// The URL template matches
/// `https://downloads.tableau.com/tssoftware/tableauhyperapi-java-<platform>-release-main.<version>.<build_id>.zip`.
#[must_use]
pub fn build_download_url(release: &PinnedRelease, platform: Platform) -> String {
    format!(
        "{base}/tableauhyperapi-java-{plat}-release-main.{version}.{build_id}.zip",
        base = BASE_URL,
        plat = platform.slug(),
        version = release.version,
        build_id = release.build_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn url_matches_expected_template() {
        let r = PinnedRelease {
            version: "0.0.24457".to_string(),
            build_id: "rc36858b6".to_string(),
            sha256: HashMap::new(),
        };
        let url = build_download_url(&r, Platform::MacosArm64);
        assert_eq!(
            url,
            "https://downloads.tableau.com/tssoftware/tableauhyperapi-java-macos-arm64-release-main.0.0.24457.rc36858b6.zip"
        );
    }
}
