// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Builds download URLs for the PyPI `tableauhyperapi` wheels, which is where
//! this crate sources the `hyperd` executable.
//!
//! We use the wheel rather than Tableau's Java/C++ API zip on
//! `downloads.tableau.com` because the wheel filename is fully constructible
//! from the release version plus a platform tag. The zip filenames embed an
//! opaque build id (e.g. `r96880f6a`) that cannot be derived from the version,
//! which previously forced every bump through an HTML scraper. The `hyperd`
//! inside the wheel is the same build — byte-identical to the one the Java
//! bundle ships — so this is a change of envelope, not of engine.
//!
//! The legacy `/packages/py3/t/<project>/<filename>` path used here is
//! constructible without querying the PyPI index; it 302-redirects to the
//! content-addressed URL. PyPI also publishes a sha256 per file, which is
//! where the digests in `hyperd-version.toml` come from.

use crate::Error;
use crate::platform::Platform;
use crate::release::PinnedRelease;

const BASE_URL: &str = "https://files.pythonhosted.org/packages/py3/t/tableauhyperapi";

/// Builds the wheel file name for the given release / platform combination,
/// for example `tableauhyperapi-0.0.26479-py3-none-macosx_13_0_arm64.whl`.
///
/// # Errors
///
/// Returns [`Error::MissingWheelTag`] if the release does not pin a wheel tag
/// for `platform`. The tag is pin data (see [`PinnedRelease::wheel_tag_for`])
/// precisely so that a missing or changed tag fails loudly here instead of
/// producing a 404 at download time.
pub fn wheel_filename(release: &PinnedRelease, platform: Platform) -> Result<String, Error> {
    let tag = release
        .wheel_tag_for(platform)
        .ok_or_else(|| Error::missing_wheel_tag(platform))?;
    Ok(format!(
        "tableauhyperapi-{version}-py3-none-{tag}.whl",
        version = release.version,
    ))
}

/// Builds the `files.pythonhosted.org` URL for the given release / platform
/// combination.
///
/// # Errors
///
/// Returns [`Error::MissingWheelTag`] if the release does not pin a wheel tag
/// for `platform`.
pub fn build_download_url(release: &PinnedRelease, platform: Platform) -> Result<String, Error> {
    Ok(format!(
        "{BASE_URL}/{file}",
        file = wheel_filename(release, platform)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn release() -> PinnedRelease {
        PinnedRelease {
            version: "0.0.26479".to_string(),
            wheel_tag: HashMap::from([
                (Platform::MacosArm64, "macosx_13_0_arm64".to_string()),
                (Platform::MacosX86_64, "macosx_10_11_x86_64".to_string()),
                (Platform::LinuxX86_64, "manylinux2014_x86_64".to_string()),
                (Platform::WindowsX86_64, "win_amd64".to_string()),
            ]),
            sha256: HashMap::new(),
        }
    }

    #[test]
    fn url_matches_expected_template() {
        assert_eq!(
            build_download_url(&release(), Platform::MacosArm64).unwrap(),
            "https://files.pythonhosted.org/packages/py3/t/tableauhyperapi/tableauhyperapi-0.0.26479-py3-none-macosx_13_0_arm64.whl"
        );
    }

    #[test]
    fn every_platform_gets_its_own_tag() {
        let r = release();
        for (platform, tag) in [
            (Platform::MacosArm64, "macosx_13_0_arm64"),
            (Platform::MacosX86_64, "macosx_10_11_x86_64"),
            (Platform::LinuxX86_64, "manylinux2014_x86_64"),
            (Platform::WindowsX86_64, "win_amd64"),
        ] {
            assert_eq!(
                wheel_filename(&r, platform).unwrap(),
                format!("tableauhyperapi-0.0.26479-py3-none-{tag}.whl")
            );
        }
    }

    #[test]
    fn missing_wheel_tag_is_an_error_not_a_guess() {
        let r = PinnedRelease {
            version: "0.0.26479".to_string(),
            wheel_tag: HashMap::new(),
            sha256: HashMap::new(),
        };
        assert!(matches!(
            build_download_url(&r, Platform::LinuxX86_64),
            Err(Error::MissingWheelTag { .. })
        ));
    }

    #[test]
    fn builtin_pin_builds_a_url_for_every_platform() {
        let r = PinnedRelease::builtin();
        for p in [
            Platform::MacosArm64,
            Platform::MacosX86_64,
            Platform::LinuxX86_64,
            Platform::WindowsX86_64,
        ] {
            let url = build_download_url(&r, p).expect("builtin pin has every wheel tag");
            assert!(url.starts_with(BASE_URL));
            assert!(
                std::path::Path::new(&url)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
            );
            assert!(url.contains(&r.version));
        }
    }
}
