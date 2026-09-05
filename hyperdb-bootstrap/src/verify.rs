// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Checks that a pinned release is still fetchable *and* still the bytes we
//! pinned. Used by the `verify` CLI subcommand and by CI workflows that guard
//! against silent yanks, URL-scheme changes, and digest drift.
//!
//! Two independent checks run per platform:
//!
//! 1. **Reachability** — HEAD the constructed
//!    `files.pythonhosted.org/packages/py3/...` URL. This is the legacy PyPI
//!    path, which is constructible without an index query but only reaches the
//!    file via a redirect, so it is worth probing directly.
//! 2. **Digest** — cross-check the pinned sha256 against the digest PyPI
//!    publishes for that exact wheel file name. This is the stronger check:
//!    reachability only proves the CDN serves *something*, whereas the digest
//!    proves the pinned bytes are the published bytes. It also catches a stale
//!    `[wheel_tag]`, which would otherwise 404 silently on one platform only.

use std::collections::HashMap;
use std::fmt;
use std::process::Command;

use serde::Deserialize;

use crate::Error;
use crate::platform::Platform;
use crate::release::PinnedRelease;
use crate::url::{build_download_url, wheel_filename};

const PLATFORMS: &[Platform] = &[
    Platform::MacosArm64,
    Platform::MacosX86_64,
    Platform::LinuxX86_64,
    Platform::WindowsX86_64,
];

/// Outcome of cross-checking one platform's pinned digest against PyPI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DigestStatus {
    /// The pinned digest matches the one PyPI publishes for the wheel.
    Match,
    /// PyPI publishes a different digest for the pinned file name.
    Mismatch {
        /// Digest PyPI publishes for the wheel.
        published: String,
    },
    /// PyPI's file list has no entry for the pinned wheel file name —
    /// typically a stale `[wheel_tag]` or a yanked release.
    FileMissing,
    /// The pin carries no digest for this platform, so there is nothing to
    /// compare. Not a failure: an empty digest is documented as "skip".
    NotPinned,
    /// The PyPI index could not be consulted. Not treated as a failure, since
    /// an index outage says nothing about the pin.
    Unknown(String),
}

impl DigestStatus {
    /// Returns `true` unless PyPI actively contradicts the pin.
    ///
    /// [`Self::Unknown`] and [`Self::NotPinned`] are "no information", not
    /// failure; [`Self::Mismatch`] and [`Self::FileMissing`] are hard failures.
    #[must_use]
    pub fn ok(&self) -> bool {
        !matches!(self, Self::Mismatch { .. } | Self::FileMissing)
    }
}

impl fmt::Display for DigestStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Match => f.write_str("digest matches PyPI"),
            Self::Mismatch { published } => {
                write!(f, "digest MISMATCH — PyPI publishes {published}")
            }
            Self::FileMissing => f.write_str("PyPI has no file with this name (stale wheel tag?)"),
            Self::NotPinned => f.write_str("no digest pinned"),
            Self::Unknown(why) => write!(f, "digest unchecked ({why})"),
        }
    }
}

/// Result of a single platform's probe performed by [`verify_release`].
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    /// Platform the probe targeted.
    pub platform: Platform,
    /// URL that was probed.
    pub url: String,
    /// HTTP status returned by the CDN, or `None` if the probe itself failed.
    pub status: Option<u16>,
    /// Error message when `status` is `None` (spawn failure, parse failure,
    /// stderr from a failed `curl` invocation, or URL construction failure).
    pub error: Option<String>,
    /// How the pinned digest compares to the one PyPI publishes.
    pub digest: DigestStatus,
}

impl VerifyOutcome {
    /// Returns `true` when the probe observed a 2xx/3xx HTTP status (what
    /// `curl --head --location` returns when the CDN serves the file) and the
    /// digest check did not contradict the pin.
    #[must_use]
    pub fn ok(&self) -> bool {
        matches!(self.status, Some(s) if (200..400).contains(&s)) && self.digest.ok()
    }
}

/// Probe every supported platform for `release`: HEAD its wheel URL and
/// cross-check its pinned digest against PyPI. Returns one outcome per
/// platform; callers decide how to surface failures.
///
/// Uses `curl` rather than an in-process HTTP client so that Akamai-style bot
/// protection (which blocks some TLS fingerprints from CI runner IPs) does not
/// cause false failures, and so the crate needs no TLS stack of its own.
///
/// # Errors
///
/// Currently always returns `Ok(_)` — individual platform failures are
/// surfaced through [`VerifyOutcome::error`] and [`VerifyOutcome::digest`].
/// The `Result` wrapper is kept for forward compatibility if a top-level
/// failure mode is added.
pub fn verify_release(release: &PinnedRelease) -> Result<Vec<VerifyOutcome>, Error> {
    let digests = digest_statuses(release);
    let mut out = Vec::with_capacity(PLATFORMS.len());
    for &platform in PLATFORMS {
        let digest = digests
            .get(&platform)
            .cloned()
            .unwrap_or_else(|| DigestStatus::Unknown("platform not probed".to_string()));
        let outcome = match build_download_url(release, platform) {
            Ok(url) => {
                let (status, error) = curl_head(&url);
                VerifyOutcome {
                    platform,
                    url,
                    status,
                    error,
                    digest,
                }
            }
            Err(e) => VerifyOutcome {
                platform,
                url: String::from("<no URL: missing wheel tag>"),
                status: None,
                error: Some(e.to_string()),
                digest,
            },
        };
        out.push(outcome);
    }
    Ok(out)
}

/// Fetch PyPI's file list for the pinned version and compare each platform's
/// pinned digest against it.
fn digest_statuses(release: &PinnedRelease) -> HashMap<Platform, DigestStatus> {
    let url = format!(
        "https://pypi.org/pypi/tableauhyperapi/{}/json",
        release.version
    );
    match curl_body(&url) {
        Ok(body) => compare_digests(&body, release),
        Err(why) => PLATFORMS
            .iter()
            .map(|&p| (p, DigestStatus::Unknown(why.clone())))
            .collect(),
    }
}

/// The subset of PyPI's per-version JSON payload we care about. Unknown
/// fields are ignored, so this survives additions to the API response.
#[derive(Deserialize)]
struct PypiVersion {
    urls: Vec<PypiFile>,
}

#[derive(Deserialize)]
struct PypiFile {
    filename: String,
    digests: PypiDigests,
}

#[derive(Deserialize)]
struct PypiDigests {
    sha256: String,
}

/// Pure comparison of a PyPI per-version JSON payload against a pin. Split out
/// from the network fetch so it can be tested against a fixture.
fn compare_digests(index_json: &str, release: &PinnedRelease) -> HashMap<Platform, DigestStatus> {
    let parsed: PypiVersion = match serde_json::from_str(index_json) {
        Ok(p) => p,
        Err(e) => {
            let why = format!("could not parse PyPI response: {e}");
            return PLATFORMS
                .iter()
                .map(|&p| (p, DigestStatus::Unknown(why.clone())))
                .collect();
        }
    };
    let published: HashMap<&str, &str> = parsed
        .urls
        .iter()
        .map(|f| (f.filename.as_str(), f.digests.sha256.as_str()))
        .collect();

    PLATFORMS
        .iter()
        .map(|&platform| {
            let status = match wheel_filename(release, platform) {
                Err(e) => DigestStatus::Unknown(e.to_string()),
                Ok(filename) => match (
                    published.get(filename.as_str()),
                    release.sha256_for(platform),
                ) {
                    (None, _) => DigestStatus::FileMissing,
                    (Some(_), None) => DigestStatus::NotPinned,
                    (Some(actual), Some(pinned)) if actual.eq_ignore_ascii_case(pinned) => {
                        DigestStatus::Match
                    }
                    (Some(actual), Some(_)) => DigestStatus::Mismatch {
                        published: (*actual).to_string(),
                    },
                },
            };
            (platform, status)
        })
        .collect()
}

/// Run `curl --head --silent --show-error --location <url>` and parse the
/// HTTP status from the first status line. Returns `(Some(status), None)`
/// on success and `(None, Some(error))` on spawn/parse failure.
fn curl_head(url: &str) -> (Option<u16>, Option<String>) {
    let result = Command::new("curl")
        .args(["--head", "--silent", "--show-error", "--location"])
        .arg(url)
        .output();

    match result {
        Err(e) => (None, Some(format!("failed to spawn curl: {e}"))),
        Ok(output) => {
            if !output.status.success() && output.stdout.is_empty() {
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                return (None, Some(stderr));
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            match last_http_status(&stdout) {
                Some(s) => (Some(s), None),
                None => (
                    None,
                    Some(format!(
                        "could not parse HTTP status from curl output: {}",
                        stdout.chars().take(200).collect::<String>()
                    )),
                ),
            }
        }
    }
}

/// Parse the last `HTTP/x.x NNN` status line. `curl --location` emits one per
/// hop, and the final hop is the one that matters.
fn last_http_status(stdout: &str) -> Option<u16> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            // Matches "HTTP/1.1 200 OK", "HTTP/2 403", etc.
            if line.starts_with("HTTP/") {
                line.split_whitespace().nth(1)?.parse::<u16>().ok()
            } else {
                None
            }
        })
        .next_back()
}

/// GET `url` with `curl` and return the response body.
fn curl_body(url: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args(["--silent", "--show-error", "--location", "--fail"])
        .arg(url)
        .output()
        .map_err(|e| format!("failed to spawn curl: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "curl exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed-down shape of a real `pypi.org/pypi/tableauhyperapi/<v>/json`
    /// response: only the fields `compare_digests` reads, plus an extra field
    /// to prove unknown keys are tolerated.
    const INDEX_JSON: &str = r#"{
      "info": {"version": "0.0.26479"},
      "urls": [
        {"filename": "tableauhyperapi-0.0.26479-py3-none-macosx_13_0_arm64.whl",
         "digests": {"sha256": "e80e4dac6d8437ad8c20f36add7e523b18bc06d90d4c605a256c57df8df2c118", "md5": "x"},
         "size": 80316638},
        {"filename": "tableauhyperapi-0.0.26479-py3-none-manylinux2014_x86_64.whl",
         "digests": {"sha256": "9f5ff04c0dc3c17224b7a3f36f297775f2f49aae084da84614003cd6508213bc"},
         "size": 89743672}
      ]
    }"#;

    fn pin(sha_arm64: &str) -> PinnedRelease {
        PinnedRelease {
            version: "0.0.26479".to_string(),
            wheel_tag: HashMap::from([
                (Platform::MacosArm64, "macosx_13_0_arm64".to_string()),
                (Platform::LinuxX86_64, "manylinux2014_x86_64".to_string()),
                (Platform::WindowsX86_64, "win_amd64".to_string()),
            ]),
            sha256: HashMap::from([(Platform::MacosArm64, sha_arm64.to_string())]),
        }
    }

    #[test]
    fn matching_digest_is_reported_as_match() {
        let statuses = compare_digests(
            INDEX_JSON,
            &pin("e80e4dac6d8437ad8c20f36add7e523b18bc06d90d4c605a256c57df8df2c118"),
        );
        assert_eq!(statuses[&Platform::MacosArm64], DigestStatus::Match);
        assert!(statuses[&Platform::MacosArm64].ok());
    }

    #[test]
    fn digest_comparison_is_case_insensitive() {
        let statuses = compare_digests(
            INDEX_JSON,
            &pin("E80E4DAC6D8437AD8C20F36ADD7E523B18BC06D90D4C605A256C57DF8DF2C118"),
        );
        assert_eq!(statuses[&Platform::MacosArm64], DigestStatus::Match);
    }

    #[test]
    fn drifted_digest_is_a_failure() {
        let statuses = compare_digests(INDEX_JSON, &pin(&"a".repeat(64)));
        assert_eq!(
            statuses[&Platform::MacosArm64],
            DigestStatus::Mismatch {
                published: "e80e4dac6d8437ad8c20f36add7e523b18bc06d90d4c605a256c57df8df2c118"
                    .to_string()
            }
        );
        assert!(!statuses[&Platform::MacosArm64].ok());
    }

    /// A wheel tag that PyPI does not publish for this version is the silent-404
    /// vector the `[wheel_tag]` pin exists to make visible.
    #[test]
    fn unpublished_wheel_tag_is_a_failure() {
        let statuses = compare_digests(INDEX_JSON, &pin(&"a".repeat(64)));
        assert_eq!(
            statuses[&Platform::WindowsX86_64],
            DigestStatus::FileMissing
        );
        assert!(!statuses[&Platform::WindowsX86_64].ok());
    }

    #[test]
    fn published_but_unpinned_digest_is_not_a_failure() {
        let statuses = compare_digests(INDEX_JSON, &pin(&"a".repeat(64)));
        assert_eq!(statuses[&Platform::LinuxX86_64], DigestStatus::NotPinned);
        assert!(statuses[&Platform::LinuxX86_64].ok());
    }

    /// A platform with no pinned wheel tag can't be checked, but must not be
    /// reported as a digest failure — `verify_release` surfaces the missing tag
    /// through the URL-construction error instead.
    #[test]
    fn missing_wheel_tag_is_unknown_not_failure() {
        let statuses = compare_digests(INDEX_JSON, &pin(&"a".repeat(64)));
        assert!(matches!(
            statuses[&Platform::MacosX86_64],
            DigestStatus::Unknown(_)
        ));
        assert!(statuses[&Platform::MacosX86_64].ok());
    }

    #[test]
    fn unparseable_response_is_unknown_for_every_platform() {
        let statuses = compare_digests("<html>503</html>", &pin(&"a".repeat(64)));
        assert_eq!(statuses.len(), PLATFORMS.len());
        for status in statuses.values() {
            assert!(matches!(status, DigestStatus::Unknown(_)));
            assert!(status.ok());
        }
    }

    #[test]
    fn last_status_line_wins_across_redirects() {
        let stdout = "HTTP/2 302\r\nlocation: elsewhere\r\n\r\nHTTP/2 200\r\n";
        assert_eq!(last_http_status(stdout), Some(200));
        assert_eq!(last_http_status("no status here"), None);
    }

    #[test]
    fn outcome_ok_requires_both_reachability_and_digest() {
        let base = VerifyOutcome {
            platform: Platform::MacosArm64,
            url: "https://example.invalid/x.whl".to_string(),
            status: Some(200),
            error: None,
            digest: DigestStatus::Match,
        };
        assert!(base.ok());
        assert!(
            !VerifyOutcome {
                status: Some(404),
                ..base.clone()
            }
            .ok()
        );
        assert!(
            !VerifyOutcome {
                digest: DigestStatus::FileMissing,
                ..base.clone()
            }
            .ok()
        );
        // An index outage must not fail an otherwise-reachable pin.
        assert!(
            VerifyOutcome {
                digest: DigestStatus::Unknown("offline".to_string()),
                ..base
            }
            .ok()
        );
    }
}
