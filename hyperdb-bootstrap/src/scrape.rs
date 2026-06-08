// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Best-effort scraper for the public Hyper releases page.
//!
//! When no version pin is supplied, [`crate::scrape::scrape_latest`] fetches
//! `https://tableau.github.io/hyper-db/docs/releases` and parses out the
//! most recent version + build id for the given platform. This bypasses
//! the compile-time pin and can lag or break when the page layout changes
//! — prefer an explicit [`crate::VersionSource::Builtin`] or
//! [`crate::VersionSource::TomlFile`] in production.

use regex::Regex;
use std::collections::HashMap;

use crate::platform::Platform;
use crate::release::PinnedRelease;
use crate::Error;

const RELEASES_URL: &str = "https://tableau.github.io/hyper-db/docs/releases";

/// Fetches the public releases page and returns the newest `PinnedRelease`
/// that has a Java download for `platform`.
///
/// The returned `PinnedRelease` has an empty SHA-256 map — scraping only
/// recovers the version + build id, never a digest.
///
/// # Errors
///
/// - [`Error::Http`] on network / TLS failure.
/// - [`Error::HttpStatus`] on a non-success HTTP response.
/// - [`Error::ScrapeFailed`] when the page layout no longer matches the
///   expected structure.
pub fn scrape_latest(platform: Platform) -> Result<PinnedRelease, Error> {
    tracing::info!(url = RELEASES_URL, "scraping latest release");
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("hyperd-bootstrap/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(Error::Http)?;
    let resp = client.get(RELEASES_URL).send().map_err(Error::Http)?;
    if !resp.status().is_success() {
        return Err(Error::http_status(RELEASES_URL, resp.status().as_u16()));
    }
    let html = resp.text().map_err(Error::Http)?;
    parse_latest(&html, platform)
}

fn parse_latest(html: &str, platform: Platform) -> Result<PinnedRelease, Error> {
    // The releases page lists entries in reverse-chronological order as
    // `<h3>VERSION [DATE]</h3>`. The first match is the newest.
    let h3_re =
        Regex::new(r"<h3[^>]*>\s*([0-9]+(?:\.[0-9]+){1,3})\s*\[[^\]]+\]").expect("valid regex");
    let version = h3_re
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or(Error::ScrapeFailed(
            "no <h3>VERSION [DATE]</h3> heading found",
        ))?;

    // For that version, find the Java zip for the requested platform to
    // recover the build id.
    let href_re = Regex::new(&format!(
        r"tableauhyperapi-java-{plat}-release-main\.{ver}\.(rc[a-z0-9]+)\.zip",
        plat = regex::escape(platform.slug()),
        ver = regex::escape(&version),
    ))
    .expect("valid regex");
    let build_id = href_re
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or(Error::ScrapeFailed(
            "no matching java zip href for scraped version",
        ))?;

    Ok(PinnedRelease {
        version,
        build_id,
        sha256: HashMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_real_page_snippet() {
        let html = r#"
            <h3>0.0.24457 [February 12 2026]</h3>
            <ul><li>Release notes</li></ul>
            <a href="https://downloads.tableau.com/tssoftware//tableauhyperapi-java-macos-arm64-release-main.0.0.24457.rc36858b6.zip">Java (macOS arm64)</a>
            <a href="https://downloads.tableau.com/tssoftware//tableauhyperapi-java-linux-x86_64-release-main.0.0.24457.rc36858b6.zip">Java (Linux)</a>
            <h3>0.0.20000 [January 1 2025]</h3>
        "#;
        let release = parse_latest(html, Platform::MacosArm64).unwrap();
        assert_eq!(release.version, "0.0.24457");
        assert_eq!(release.build_id, "rc36858b6");
    }

    #[test]
    fn parse_errors_when_no_heading() {
        let html = "<p>nothing here</p>";
        assert!(matches!(
            parse_latest(html, Platform::LinuxX86_64),
            Err(Error::ScrapeFailed(_))
        ));
    }

    #[test]
    fn parse_errors_when_no_matching_href() {
        let html = r"<h3>0.0.24457 [Feb 12 2026]</h3>";
        assert!(matches!(
            parse_latest(html, Platform::WindowsX86_64),
            Err(Error::ScrapeFailed(_))
        ));
    }
}
