// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for hyperd-bootstrap URL and install flows.

use hyperdb_bootstrap::{
    install, url::build_download_url, InstallOptions, PinnedRelease, Platform, VersionSource,
};

#[test]
fn builtin_release_builds_a_valid_url() {
    let r = PinnedRelease::builtin();
    let url = build_download_url(&r, Platform::LinuxX86_64);
    assert!(url.starts_with("https://downloads.tableau.com/tssoftware/"));
    assert!(url.contains("java-linux-x86_64"));
    assert!(std::path::Path::new(&url)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip")));
    assert!(url.contains(&r.version));
    assert!(url.contains(&r.build_id));
}

#[test]
fn install_options_defaults_are_sensible() {
    let opts = InstallOptions::default();
    assert_eq!(opts.dest_root, std::path::Path::new(".hyperd"));
    assert!(matches!(opts.version_source, VersionSource::Builtin));
    assert!(!opts.force);
    assert!(opts.platform.is_none());
}

#[test]
#[ignore = "hits the public Tableau downloads CDN; run with --ignored"]
fn install_end_to_end_with_builtin() {
    let tmp = tempfile::tempdir().unwrap();
    let installed = install(InstallOptions {
        dest_root: tmp.path().to_path_buf(),
        version_source: VersionSource::Builtin,
        platform: None,
        force: false,
    })
    .expect("install should succeed");
    assert!(installed.binary_path.exists());
    assert!(installed
        .binary_path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("hyperd")));
}

#[test]
#[ignore = "scrapes a live web page; run with --ignored"]
fn scrape_latest_real_page() {
    let platform = Platform::current().expect("supported platform");
    let r = hyperdb_bootstrap::scrape::scrape_latest(platform).expect("scrape succeeds");
    assert!(!r.version.is_empty());
    assert!(r.build_id.starts_with("rc"));
}
