// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compile-time sanity check for the pinned release metadata.
//!
//! If `hyperd-version.toml` fails to parse, is missing `version`/`build_id`,
//! or has a shape the runtime code can't handle, the build fails here —
//! before any contributor ships a broken bump.
//!
//! Keep this in sync with `release.rs` / `platform.rs`. The check is
//! deliberately lightweight (no network, no crates-io deps beyond what
//! the library already uses).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let pin_path = manifest_dir.join("hyperd-version.toml");

    // Rebuild whenever the pin or this script changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", pin_path.display());

    // Capture the git short hash for `--version` output. Falls back to
    // "unknown" when source was obtained as a tarball or git is missing.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads");
    println!("cargo:rerun-if-changed=../.git/index");
    let hash = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "-uno"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());
    let git_hash = if dirty && hash != "unknown" {
        format!("{hash}-dirty")
    } else {
        hash
    };
    println!("cargo:rustc-env=HYPERDB_GIT_HASH={git_hash}");

    let text = std::fs::read_to_string(&pin_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", pin_path.display()));

    let pin: PinCheck =
        toml::from_str(&text).unwrap_or_else(|e| panic!("invalid {}: {e}", pin_path.display()));

    assert!(
        !pin.version.trim().is_empty(),
        "{}: `version` is empty",
        pin_path.display()
    );
    assert!(
        !pin.build_id.trim().is_empty(),
        "{}: `build_id` is empty",
        pin_path.display()
    );
    // Reject stray whitespace / accidental newlines in the URL components.
    assert!(
        !pin.version.contains(char::is_whitespace),
        "{}: `version` contains whitespace",
        pin_path.display()
    );
    assert!(
        !pin.build_id.contains(char::is_whitespace),
        "{}: `build_id` contains whitespace",
        pin_path.display()
    );

    const SUPPORTED: &[&str] = &[
        "macos-arm64",
        "macos-x86_64",
        "linux-x86_64",
        "windows-x86_64",
    ];
    for key in pin.sha256.keys() {
        assert!(
            SUPPORTED.contains(&key.as_str()),
            "{}: unknown platform key `{}` in [sha256]; supported: {:?}",
            pin_path.display(),
            key,
            SUPPORTED
        );
    }
    for (plat, sha) in &pin.sha256 {
        let trimmed = sha.trim();
        if trimmed.is_empty() {
            // Empty = skip verification for that platform (documented
            // behavior). Surface it as a compile-time warning so nobody
            // ships a release pin with missing hashes by accident.
            println!(
                "cargo:warning=hyperd-version.toml: sha256 for `{plat}` is empty; downloads will skip verification"
            );
            continue;
        }
        assert!(
            trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()),
            "{}: sha256 for `{}` is not a 64-char hex string (got {:?})",
            pin_path.display(),
            plat,
            trimmed
        );
    }
}

#[derive(serde::Deserialize)]
struct PinCheck {
    version: String,
    build_id: String,
    #[serde(default)]
    sha256: HashMap<String, String>,
}
