// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The single-instance lock of the daemon.
//!
//! A daemon holds an exclusive lock on `<state dir>/daemon.lock` for its whole
//! life, until after `hyperd` has exited. Holding it is what "a daemon is
//! alive" means: unlike the discovery record it cannot go stale, because the
//! operating system releases it when the holding process exits, however it
//! exits. A client that finds the lock held and no usable record knows a
//! daemon is starting or stopping, and waits instead of spawning a second one.
//!
//! **The lock file is never unlinked.** A process holding the lock on a
//! deleted file would no longer exclude a newcomer, which creates a fresh file
//! and locks that one. Releasing the lock only closes the handle.
//!
//! The lock conflicts within one process too, because each acquire opens the
//! file separately; the in-process test daemons rely on that.
//!
//! This module depends only on `std`, `libc` and `windows-sys`, so it can be
//! compiled for Windows on its own.

use std::fs::File;
use std::io;
use std::path::Path;

/// File name of the lock inside the state directory.
pub const LOCK_FILE_NAME: &str = "daemon.lock";

/// An exclusive hold on the daemon lock. Released when dropped.
#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    /// Try to take the daemon lock in `state_dir` without waiting.
    ///
    /// Returns `Ok(None)` when another holder (in this process or another)
    /// has it. The state directory must already exist.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the lock file cannot be opened or locked for
    /// any reason other than being held, such as `state_dir` missing, or the
    /// lock file being a symlink on Unix.
    pub fn try_acquire(state_dir: &Path) -> io::Result<Option<Self>> {
        platform::try_lock(&state_dir.join(LOCK_FILE_NAME))
            .map(|held| held.map(|file| Self { _file: file }))
    }
}

#[cfg(unix)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub(super) fn try_lock(path: &Path) -> io::Result<Option<File>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        // SAFETY: `file` owns a valid open descriptor for the whole call.
        // `flock` reads no memory.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    /// `ERROR_SHARING_VIOLATION`: another handle has the file open.
    const ERROR_SHARING_VIOLATION: i32 = 32;

    /// Opening with no sharing is the lock: a second open fails while the
    /// first handle lives.
    pub(super) fn try_lock(path: &Path) -> io::Result<Option<File>> {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
        {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DaemonLock, LOCK_FILE_NAME};

    #[test]
    fn second_acquire_in_the_same_process_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = DaemonLock::try_acquire(dir.path())
            .expect("acquire")
            .expect("free lock");
        assert!(
            DaemonLock::try_acquire(dir.path())
                .expect("second acquire")
                .is_none(),
            "a held lock must refuse a second holder"
        );
        drop(first);
    }

    #[test]
    fn released_on_drop_and_the_file_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = DaemonLock::try_acquire(dir.path())
            .expect("acquire")
            .expect("free lock");
        drop(lock);
        assert!(
            dir.path().join(LOCK_FILE_NAME).exists(),
            "the lock file is never unlinked"
        );
        assert!(
            DaemonLock::try_acquire(dir.path())
                .expect("reacquire")
                .is_some(),
            "a released lock can be taken again"
        );
    }

    #[test]
    fn missing_state_dir_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing");
        assert!(DaemonLock::try_acquire(&missing).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_lock_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"").expect("write target");
        std::os::unix::fs::symlink(&target, dir.path().join(LOCK_FILE_NAME)).expect("symlink");
        assert!(
            DaemonLock::try_acquire(dir.path()).is_err(),
            "O_NOFOLLOW must refuse a symlink planted at the lock path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let _lock = DaemonLock::try_acquire(dir.path())
            .expect("acquire")
            .expect("free lock");
        let mode = std::fs::metadata(dir.path().join(LOCK_FILE_NAME))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
