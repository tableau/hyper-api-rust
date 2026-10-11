// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! File identity: whether two paths name the same file on disk.
//!
//! Comparing paths misses symlinks, hard links and case-folding file
//! systems; comparing identities does not. Unix uses `(st_dev, st_ino)`.
//! Windows uses the volume serial number and file index from
//! `GetFileInformationByHandle`, opening the file with no access rights so
//! that a file `hyperd` holds open exclusively can still be identified.
//!
//! This module depends only on `std` and `windows-sys`, so it can be
//! compiled for Windows on its own.

use std::io;
use std::path::Path;

/// The identity of an existing file or directory. Equal identities are the
/// same file, whatever paths reached them. Symlinks are followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    volume: u64,
    index: u64,
}

impl FileId {
    /// The identity of the file `path` names.
    ///
    /// # Errors
    ///
    /// Returns the I/O error from looking the file up, `NotFound` when it
    /// does not exist.
    #[cfg(unix)]
    pub(crate) fn of(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            volume: metadata.dev(),
            index: metadata.ino(),
        })
    }

    /// The identity of the file `path` names.
    ///
    /// # Errors
    ///
    /// Returns the I/O error from opening the file, `NotFound` when it does
    /// not exist.
    #[cfg(windows)]
    pub(crate) fn of(path: &Path) -> io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        };

        // No access rights: reading metadata then does not conflict with
        // another process's share mode. `FILE_FLAG_BACKUP_SEMANTICS` is
        // required to open a directory.
        let file = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        // SAFETY: `file` is open for the duration of the call, and `info`
        // points to writable storage of the type the call fills.
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, so it initialized `info`.
        let info = unsafe { info.assume_init() };
        Ok(Self {
            volume: u64::from(info.dwVolumeSerialNumber),
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::FileId;

    #[test]
    fn same_file_through_another_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"a").expect("write a");
        std::fs::write(&b, b"b").expect("write b");
        let id = FileId::of(&a).expect("id of a");
        assert_eq!(id, FileId::of(&dir.path().join(".").join("a")).expect("id"));
        assert_ne!(id, FileId::of(&b).expect("id of b"));
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&a, &link).expect("symlink");
            assert_eq!(id, FileId::of(&link).expect("id of link"));
            let hard = dir.path().join("hard");
            std::fs::hard_link(&a, &hard).expect("hard link");
            assert_eq!(id, FileId::of(&hard).expect("id of hard link"));
        }
    }

    #[test]
    fn missing_file_is_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = FileId::of(&dir.path().join("missing")).expect_err("missing");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
