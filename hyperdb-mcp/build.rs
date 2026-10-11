// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Build-time capture of the current git commit hash.
//!
//! Runs `git rev-parse --short=8 HEAD` and exposes the result as the
//! `HYPERDB_GIT_HASH` compile-time env var, consumed via `env!` in
//! `src/version.rs`. Also detects whether the working tree has
//! uncommitted changes and, when so, appends both a `-dirty` marker
//! **and** an ISO 8601 basic UTC build timestamp (e.g.
//! `-20260423T184900Z`) so two iterative builds of the same dirty
//! tree can still be told apart — the hash alone stops being unique
//! the moment uncommitted edits enter the picture.
//!
//! The timestamp shape is deliberate:
//!
//! * Real ISO 8601 basic format (no colons or dashes inside the
//!   datetime), which is safe in filenames, URLs, and version
//!   identifiers everywhere we might carry it (logs, status output,
//!   bug reports).
//! * `T` separator and `Z` suffix make it unambiguously UTC and
//!   obviously a timestamp — not another commit hash.
//! * Sortable lexicographically, so later rebuilds naturally sort
//!   after earlier ones even when only the timestamp differs.
//!
//! Clean builds are left alone: the commit hash already uniquely
//! identifies them, so no timestamp is appended.
//!
//! Falls back to `unknown` on any failure (no git binary, not a repo,
//! etc.) so the crate still builds for consumers who
//! obtained the source as a tarball.
//!
//! `Cargo.lock` is excluded from the dirty check via a git pathspec —
//! see the inline comment by the `git status` invocation for why.

use std::process::Command;

fn main() {
    // Always re-run when HEAD moves so the embedded hash stays fresh.
    // `rerun-if-changed` is path-relative to the crate root, so reach up
    // one directory into the workspace `.git`.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads");
    // Staging or unstaging rewrites `.git/index`, which reruns this script.
    // Unstaged edits to tracked files do not, so the `-dirty` marker is
    // best-effort until HEAD, a branch ref, or the index next changes.
    println!("cargo:rerun-if-changed=../.git/index");

    let hash = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    // Only count tracked modifications (`-uno` = no untracked files).
    // Otherwise uncommitted `.cursor/` configs or IDE scratch files would
    // permanently mark every build dirty even though the source tree
    // itself matches HEAD.
    //
    // `Cargo.lock` is excluded from the dirty check via a pathspec. The
    // release-please workflow syncs `Cargo.lock` in the release PR; this
    // exclusion is a safety net for a release commit whose lockfile is still
    // stale, which `cargo build` would rewrite in place. Any other modified
    // tracked file, including a `Cargo.lock` edited alongside a code change,
    // still trips the marker.
    //
    // `:(exclude,top)` is git's portable long-form pathspec magic:
    // `top` anchors at the repo root (so a hypothetical sub-crate
    // sample lockfile wouldn't match) and `exclude` inverts. Both
    // magic words are supported on every git version GitHub Actions
    // runners ship (≥ 2.39).
    let dirty = Command::new("git")
        .args([
            "status",
            "--porcelain",
            "-uno",
            "--",
            ":(exclude,top)Cargo.lock",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());

    let combined = if dirty && hash != "unknown" {
        // ISO 8601 basic format (no separators) in UTC — see module
        // docs for rationale. `strftime` specifiers: %Y=year, %m=month,
        // %d=day, %H=hour (24-hour), %M=minute, %S=second.
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        format!("{hash}-dirty-{ts}")
    } else {
        hash
    };

    println!("cargo:rustc-env=HYPERDB_GIT_HASH={combined}");
}
