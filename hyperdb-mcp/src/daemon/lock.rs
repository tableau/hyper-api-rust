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
//! file separately (`flock` on Unix, a `LockFileEx` byte-range lock on
//! Windows, both owned by the open handle); the in-process test daemons rely
//! on that.
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
        platform::try_lock(&state_dir.join(LOCK_FILE_NAME), true)
            .map(|held| held.map(|file| Self { _file: file }))
    }

    /// Whether another holder has the lock, without ever creating the lock
    /// file: a missing file (or state directory) means nobody holds it. The
    /// probe takes the lock for an instant and lets go again.
    ///
    /// # Errors
    ///
    /// As [`Self::try_acquire`], except that a missing file is not an error.
    pub fn is_held(state_dir: &Path) -> io::Result<bool> {
        match platform::try_lock(&state_dir.join(LOCK_FILE_NAME), false) {
            Ok(held) => Ok(held.is_none()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub(super) fn try_lock(path: &Path, create: bool) -> io::Result<Option<File>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
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
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    /// `ERROR_LOCK_VIOLATION`: another handle holds a conflicting byte-range
    /// lock.
    const ERROR_LOCK_VIOLATION: i32 = 33;

    /// An exclusive `LockFileEx` lock on the first byte is the lock. Byte
    /// range locks belong to the handle, so a second open in this process
    /// conflicts like one in another process, and the OS drops the lock when
    /// the handle closes however the process exits. The file is opened
    /// without `FILE_SHARE_DELETE` so it cannot be unlinked while held.
    pub(super) fn try_lock(path: &Path, create: bool) -> io::Result<Option<File>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)?;
        // SAFETY: an all-zero `OVERLAPPED` is the documented initial state
        // (offset 0, no event) for a synchronous `LockFileEx`.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        // SAFETY: `file` owns a valid handle for the whole call and
        // `overlapped` outlives it.
        let locked = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &raw mut overlapped,
            )
        };
        if locked != 0 {
            return Ok(Some(file));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
            Ok(None)
        } else {
            Err(error)
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
    fn is_held_never_creates_the_lock_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!DaemonLock::is_held(dir.path()).expect("probe"));
        assert!(
            !dir.path().join(LOCK_FILE_NAME).exists(),
            "a probe must not create daemon.lock"
        );
        assert!(!DaemonLock::is_held(&dir.path().join("missing")).expect("missing dir is free"));

        let lock = DaemonLock::try_acquire(dir.path())
            .expect("acquire")
            .expect("free lock");
        assert!(DaemonLock::is_held(dir.path()).expect("probe"));
        drop(lock);
        assert!(!DaemonLock::is_held(dir.path()).expect("probe"));
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
