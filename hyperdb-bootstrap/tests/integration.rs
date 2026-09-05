// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for hyperd-bootstrap URL and install flows.

use hyperdb_bootstrap::{
    InstallOptions, PinnedRelease, Platform, VersionSource, install, url::build_download_url,
    url::wheel_filename,
};

const PLATFORMS: [Platform; 4] = [
    Platform::MacosArm64,
    Platform::MacosX86_64,
    Platform::LinuxX86_64,
    Platform::WindowsX86_64,
];

#[test]
fn builtin_release_builds_a_valid_wheel_url_for_every_platform() {
    let r = PinnedRelease::builtin();
    for platform in PLATFORMS {
        let url = build_download_url(&r, platform).expect("builtin pin has every wheel tag");
        assert!(
            url.starts_with("https://files.pythonhosted.org/packages/py3/t/tableauhyperapi/"),
            "unexpected base for {platform}: {url}"
        );
        assert!(
            std::path::Path::new(&url)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
        );
        assert!(url.contains(&r.version));
        assert!(url.contains(r.wheel_tag_for(platform).expect("wheel tag")));
    }
}

/// The wheel file name is what PyPI publishes a digest against, so it has to
/// match the `tableauhyperapi-<version>-py3-none-<tag>.whl` convention exactly.
#[test]
fn builtin_wheel_filenames_follow_the_pypi_convention() {
    let r = PinnedRelease::builtin();
    for platform in PLATFORMS {
        let name = wheel_filename(&r, platform).expect("builtin pin has every wheel tag");
        assert_eq!(
            name,
            format!(
                "tableauhyperapi-{}-py3-none-{}.whl",
                r.version,
                r.wheel_tag_for(platform).expect("wheel tag")
            )
        );
    }
}

/// Every platform must carry both a wheel tag and a digest, or `download`
/// breaks (or silently skips verification) on that platform alone.
#[test]
fn builtin_release_pins_every_platform() {
    let r = PinnedRelease::builtin();
    for platform in PLATFORMS {
        assert!(
            r.wheel_tag_for(platform).is_some(),
            "{platform} has no wheel tag"
        );
        let sha = r.sha256_for(platform).expect("digest");
        assert_eq!(sha.len(), 64, "{platform} digest is not 64 hex chars");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    }
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
#[ignore = "downloads an ~80 MB wheel from PyPI; run with --ignored"]
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
    assert!(
        installed
            .binary_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("hyperd"))
    );
    // The install dir and VERSION marker are keyed on the version alone.
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("current").join("VERSION")).unwrap(),
        installed.version
    );
    assert!(tmp.path().join(&installed.version).is_dir());
}

#[test]
#[ignore = "hits the PyPI JSON API and the wheel CDN; run with --ignored"]
fn verify_builtin_release_against_pypi() {
    let r = PinnedRelease::builtin();
    let outcomes = hyperdb_bootstrap::verify_release(&r).expect("verify runs");
    assert_eq!(outcomes.len(), PLATFORMS.len());
    for o in &outcomes {
        assert!(
            o.ok(),
            "{} failed: status={:?} error={:?} digest={}",
            o.platform,
            o.status,
            o.error,
            o.digest
        );
        assert_eq!(
            o.digest,
            hyperdb_bootstrap::DigestStatus::Match,
            "{} digest not confirmed against PyPI",
            o.platform
        );
    }
}
