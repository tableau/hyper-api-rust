// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Downloads the `hyperd` release archive to a temp file and verifies its
//! SHA-256 digest.
//!
//! The download itself is performed by a `curl` subprocess rather than an
//! in-process HTTP client — see the inline comment on
//! [`crate::download::download_and_verify`] for the reason.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use crate::Error;

const HASH_CHUNK: usize = 64 * 1024;

/// Downloads `url` to `dest` via `curl` and, if `expected_sha256` is
/// supplied, verifies that the downloaded bytes match.
///
/// When `expected_sha256` is `None`, the digest is still computed and
/// logged at WARN level so that humans running ad-hoc bootstraps see what
/// they got even though no pin was checked.
///
/// # Errors
///
/// - [`Error::Io`] if `curl` cannot be spawned or the output file cannot
///   be hashed.
/// - [`Error::CurlFailed`] if `curl` exits with a non-zero status.
/// - [`Error::ChecksumMismatch`] if `expected_sha256` is supplied and the
///   digest of the downloaded bytes differs.
pub fn download_and_verify(
    url: &str,
    expected_sha256: Option<&str>,
    dest: &Path,
) -> Result<(), Error> {
    tracing::info!(url, dest = %dest.display(), "downloading");

    // Use curl rather than reqwest: Akamai's bot-protection layer on
    // downloads.tableau.com blocks GitHub-hosted runner IPs when approached
    // with non-browser TLS stacks, but allows curl's well-known client hello.
    let status = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--output",
        ])
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|source| Error::io("spawning curl", source))?;

    if !status.success() {
        return Err(Error::curl_failed(url, status.code().unwrap_or(-1)));
    }

    let actual = hash_file(dest)?;
    match expected_sha256 {
        Some(expected) => {
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(Error::checksum_mismatch(expected, actual));
            }
            tracing::info!(sha256 = %actual, "sha256 verified");
        }
        None => {
            tracing::warn!(
                sha256 = %actual,
                "sha256 verification skipped (no expected hash supplied)"
            );
        }
    }
    Ok(())
}

#[expect(
    clippy::format_collect,
    reason = "readable hex/string formatting loop; refactoring to fold! obscures intent"
)]
fn hash_file(path: &Path) -> Result<String, Error> {
    let mut file = File::open(path)
        .map_err(|source| Error::io(format!("opening {} for hashing", path.display()), source))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|source| Error::io(format!("reading {}", path.display()), source))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    // sha2 0.11 returns `Array<u8, _>` from `finalize()`, which (unlike
    // the previous `GenericArray`) does not implement `LowerHex`. Iterate
    // over the byte slice and lower-hex each byte ourselves.
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}
