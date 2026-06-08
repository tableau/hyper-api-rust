// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ZIP-archive extraction for the Hyper API release bundle.
//!
//! The upstream archive nests `hyperd` plus its shared libraries inside a
//! versioned top-level directory (e.g.
//! `tableauhyperapi-java-macos-arm64-release-main.0.0.24457.rc36858b6/`) and
//! then under `lib/hyper/` on Linux/macOS or `bin/hyper/` on Windows. This
//! module flattens both layers so downstream consumers only see the
//! `hyperd` runtime files. The layout is identical across the Java and C++
//! bundles, so this extractor is agnostic to which binding we download
//! (we use Java — see `url.rs` for why).

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::Error;

/// Extract everything under `lib/hyper/` (or `bin/hyper/` on Windows) from
/// the Hyper API zip into `dest_dir`, flattening the wrapper prefixes
/// away. Returns the list of extracted file paths relative to `dest_dir`.
///
/// # Errors
///
/// Returns [`Error::Io`] for filesystem failures, [`Error::Zip`] if the
/// archive cannot be opened or parsed, and [`Error::HyperdNotInArchive`]
/// if the archive does not contain a `hyperd` / `hyperd.exe` entry.
pub fn extract_hyperd(zip_path: &Path, dest_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let file = File::open(zip_path)
        .map_err(|source| Error::io(format!("opening zip {}", zip_path.display()), source))?;
    let mut archive = zip::ZipArchive::new(file).map_err(Error::Zip)?;

    fs::create_dir_all(dest_dir)
        .map_err(|source| Error::io(format!("creating {}", dest_dir.display()), source))?;

    let mut extracted = Vec::new();
    let mut found_hyperd = false;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(Error::Zip)?;
        let Some(enclosed) = entry.enclosed_name() else {
            continue;
        };
        let Some(rel) = strip_lib_hyper_prefix(&enclosed) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }

        let out_path = dest_dir.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|source| Error::io(format!("creating {}", out_path.display()), source))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| Error::io(format!("creating {}", parent.display()), source))?;
        }
        let mut out = File::create(&out_path)
            .map_err(|source| Error::io(format!("creating {}", out_path.display()), source))?;
        io::copy(&mut entry, &mut out)
            .map_err(|source| Error::io(format!("writing {}", out_path.display()), source))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(mode));
            }
        }

        if rel
            .file_name()
            .is_some_and(|n| n == "hyperd" || n == "hyperd.exe")
        {
            found_hyperd = true;
        }
        extracted.push(rel);
    }

    if !found_hyperd {
        return Err(Error::HyperdNotInArchive);
    }
    Ok(extracted)
}

/// Return the path stripped of a leading `lib/hyper/` or `bin/hyper/` prefix,
/// or `None` if the entry is outside those directories. The Hyper API zip
/// wraps everything in a top-level `tableauhyperapi-<binding>-...` directory and
/// nests the runtime under `lib/hyper/` (Linux/macOS) or `bin/hyper/`
/// (Windows) inside it.
fn strip_lib_hyper_prefix(path: &Path) -> Option<PathBuf> {
    let mut comps = path.components();
    // Skip one optional top-level wrapper component (e.g.
    // `tableauhyperapi-java-macos-arm64-release-main.0.0.24457.rc36858b6`)
    // before looking for the `lib/hyper` or `bin/hyper` pair.
    let first = comps.next()?;
    let (a, b) = if first.as_os_str() == "lib" || first.as_os_str() == "bin" {
        (first, comps.next()?)
    } else {
        (comps.next()?, comps.next()?)
    };
    if (a.as_os_str() == "lib" || a.as_os_str() == "bin") && b.as_os_str() == "hyper" {
        Some(comps.as_path().to_path_buf())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_prefix_matches_lib_hyper() {
        // Bare form (no wrapping top-level dir).
        assert_eq!(
            strip_lib_hyper_prefix(Path::new("lib/hyper/hyperd")),
            Some(PathBuf::from("hyperd"))
        );
        assert_eq!(
            strip_lib_hyper_prefix(Path::new("lib/hyper/sub/file")),
            Some(PathBuf::from("sub/file"))
        );
        // Real-world form: wrapped under a versioned top-level dir (Linux/macOS).
        assert_eq!(
            strip_lib_hyper_prefix(Path::new("tableauhyperapi-java-x/lib/hyper/hyperd")),
            Some(PathBuf::from("hyperd"))
        );
        assert_eq!(
            strip_lib_hyper_prefix(Path::new("tableauhyperapi-java-x/lib/hyper/sub/a.so")),
            Some(PathBuf::from("sub/a.so"))
        );
        // Windows uses bin/hyper/ instead of lib/hyper/.
        assert_eq!(
            strip_lib_hyper_prefix(Path::new("tableauhyperapi-java-x/bin/hyper/hyperd.exe")),
            Some(PathBuf::from("hyperd.exe"))
        );
        assert_eq!(
            strip_lib_hyper_prefix(Path::new("tableauhyperapi-java-x/bin/hyper/sub/extra.dll")),
            Some(PathBuf::from("sub/extra.dll"))
        );
        // Anything outside lib/hyper or bin/hyper is dropped.
        assert_eq!(
            strip_lib_hyper_prefix(Path::new("tableauhyperapi-java-x/include/foo.hpp")),
            None
        );
        assert_eq!(
            strip_lib_hyper_prefix(Path::new("tableauhyperapi-java-x/bin/tableauhyperapi.dll")),
            None
        );
        assert_eq!(strip_lib_hyper_prefix(Path::new("other/file")), None);
    }

    #[test]
    fn extract_fixture_zip() -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        let tmp = tempfile::tempdir()?;
        let zip_path = tmp.path().join("fixture.zip");
        {
            let file = File::create(&zip_path)?;
            let mut zw = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            // Mirror the real release zip's top-level wrapper dir.
            zw.start_file("tableauhyperapi-java-fake/lib/hyper/hyperd", opts)?;
            zw.write_all(b"fake hyperd")?;
            zw.start_file("tableauhyperapi-java-fake/lib/hyper/sub/extra.txt", opts)?;
            zw.write_all(b"extra")?;
            zw.start_file("tableauhyperapi-java-fake/include/ignored.hpp", opts)?;
            zw.write_all(b"nope")?;
            zw.finish()?;
        }
        let out = tmp.path().join("out");
        let files = extract_hyperd(&zip_path, &out)?;
        assert!(files.iter().any(|p| p == Path::new("hyperd")));
        assert!(files.iter().any(|p| p == Path::new("sub/extra.txt")));
        assert!(out.join("hyperd").exists());
        assert!(!out.join("ignored.hpp").exists());
        Ok(())
    }
}
