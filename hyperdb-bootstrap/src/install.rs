// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! High-level [`install()`] entry point: resolves a release, downloads and
//! verifies the archive, extracts the executable into
//! `<dest_root>/<version_tag>/`, then refreshes the
//! `<dest_root>/current/` pointer so downstream tooling can always find the
//! active install at a stable path.

use std::fs;
use std::path::{Path, PathBuf};

use crate::download::download_and_verify;
use crate::extract::extract_hyperd;
use crate::platform::Platform;
use crate::release::PinnedRelease;
use crate::scrape::scrape_latest;
use crate::url::build_download_url;
use crate::Error;

/// Default directory (relative to CWD) used when [`InstallOptions::dest_root`]
/// is left at its default. Laid out as
/// `.hyperd/<version_tag>/hyperd[.exe]` plus a mirror at `.hyperd/current/`.
pub const DEFAULT_DEST_ROOT: &str = ".hyperd";

/// Which `hyperd` release [`install`] should resolve.
///
/// The default is [`VersionSource::Builtin`], which uses the pin baked into
/// the crate at compile time. Other variants override that pin.
#[derive(Debug, Clone, Default)]
pub enum VersionSource {
    /// Use the release baked into the crate at compile time.
    #[default]
    Builtin,
    /// Load pinned metadata from a specific TOML file.
    TomlFile(PathBuf),
    /// Caller-supplied release (e.g. CLI `--version`/`--build-id` flags).
    Explicit(PinnedRelease),
    /// Best-effort scrape of the public releases page.
    ScrapeLatest,
}

/// Configuration passed to [`install`].
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Root directory under which `<version_tag>/` and `current/` are created.
    pub dest_root: PathBuf,
    /// Which release to resolve. See [`VersionSource`].
    pub version_source: VersionSource,
    /// Override the host-platform auto-detection (useful for cross-platform
    /// bootstrap in CI). `None` means "detect from the current host".
    pub platform: Option<Platform>,
    /// When `true`, re-download even if a cached install already exists.
    pub force: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            dest_root: PathBuf::from(DEFAULT_DEST_ROOT),
            version_source: VersionSource::Builtin,
            platform: None,
            force: false,
        }
    }
}

/// Result of a successful [`install`] call.
#[derive(Debug, Clone)]
pub struct InstalledHyperd {
    /// Absolute or relative path to the installed `hyperd` executable.
    /// Always under `<dest_root>/current/`.
    pub binary_path: PathBuf,
    /// Installed release version (for example, `"0.0.24457"`).
    pub version: String,
    /// Installed release build id (for example, `"rc36858b6"`).
    pub build_id: String,
    /// Host platform this install targets.
    pub platform: Platform,
    /// `true` if the versioned directory already existed and we skipped the
    /// download; `false` if we downloaded and extracted fresh bytes.
    pub cache_hit: bool,
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "call-site ergonomics: function consumes logically-owned parameters, refactoring signatures is not worth per-site churn"
)]
/// Downloads (if needed), verifies, and installs a `hyperd` executable
/// according to `opts`.
///
/// This function is **blocking**. It performs HTTP I/O, ZIP extraction, and
/// filesystem operations on the calling thread; it has no dependency on
/// `tokio`.
///
/// # Errors
///
/// Returns any [`Error`] variant produced by the phases it drives —
/// platform detection, release resolution (TOML parsing or scraping),
/// download, checksum verification, ZIP extraction, or filesystem
/// operations under `dest_root`.
pub fn install(opts: InstallOptions) -> Result<InstalledHyperd, Error> {
    let platform = match opts.platform {
        Some(p) => p,
        None => Platform::current()?,
    };
    let release = resolve_release(&opts.version_source, platform)?;
    let version_tag = release.version_tag();
    let versioned_dir = opts.dest_root.join(&version_tag);
    let current_dir = opts.dest_root.join("current");
    let exe_name = platform.executable_name();

    let cache_hit = versioned_dir.join(exe_name).exists() && !opts.force;
    if cache_hit {
        tracing::info!(
            dir = %versioned_dir.display(),
            "cache hit, skipping download"
        );
    } else {
        download_and_extract(&release, platform, &versioned_dir)?;
    }

    refresh_current(&current_dir, &versioned_dir, &version_tag)?;
    let binary_path = current_dir.join(exe_name);
    Ok(InstalledHyperd {
        binary_path,
        version: release.version,
        build_id: release.build_id,
        platform,
        cache_hit,
    })
}

fn resolve_release(source: &VersionSource, platform: Platform) -> Result<PinnedRelease, Error> {
    match source {
        VersionSource::Builtin => Ok(PinnedRelease::builtin()),
        VersionSource::TomlFile(path) => PinnedRelease::from_toml_file(path),
        VersionSource::Explicit(r) => Ok(r.clone()),
        VersionSource::ScrapeLatest => scrape_latest(platform),
    }
}

fn download_and_extract(
    release: &PinnedRelease,
    platform: Platform,
    versioned_dir: &Path,
) -> Result<(), Error> {
    if versioned_dir.exists() {
        fs::remove_dir_all(versioned_dir)
            .map_err(|source| Error::io(format!("clearing {}", versioned_dir.display()), source))?;
    }
    fs::create_dir_all(versioned_dir)
        .map_err(|source| Error::io(format!("creating {}", versioned_dir.display()), source))?;

    let url = build_download_url(release, platform);
    let tmp = tempfile::tempdir().map_err(|source| Error::io("creating temp dir", source))?;
    let zip_path = tmp.path().join("hyperapi-java.zip");
    download_and_verify(&url, release.sha256_for(platform), &zip_path)?;
    extract_hyperd(&zip_path, versioned_dir)?;
    Ok(())
}

fn refresh_current(current: &Path, source: &Path, version_tag: &str) -> Result<(), Error> {
    // current/ is a fresh file copy every run — avoids Windows symlink
    // privileges and keeps the Makefile auto-discovery path stable.
    if current.exists() {
        fs::remove_dir_all(current)
            .map_err(|source| Error::io(format!("clearing {}", current.display()), source))?;
    }
    fs::create_dir_all(current)
        .map_err(|source| Error::io(format!("creating {}", current.display()), source))?;
    copy_dir_contents(source, current)?;
    fs::write(current.join("VERSION"), version_tag)
        .map_err(|source| Error::io(format!("writing {}/VERSION", current.display()), source))?;
    Ok(())
}

fn copy_dir_contents(from: &Path, to: &Path) -> Result<(), Error> {
    for entry in fs::read_dir(from)
        .map_err(|source| Error::io(format!("reading {}", from.display()), source))?
    {
        let entry =
            entry.map_err(|source| Error::io(format!("reading {}", from.display()), source))?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let ty = entry
            .file_type()
            .map_err(|source| Error::io(format!("stat {}", src.display()), source))?;
        if ty.is_dir() {
            fs::create_dir_all(&dst)
                .map_err(|source| Error::io(format!("creating {}", dst.display()), source))?;
            copy_dir_contents(&src, &dst)?;
        } else {
            fs::copy(&src, &dst).map_err(|source| {
                Error::io(
                    format!("copying {} -> {}", src.display(), dst.display()),
                    source,
                )
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&src) {
                    let mode = meta.permissions().mode();
                    let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(mode));
                }
            }
        }
    }
    Ok(())
}
