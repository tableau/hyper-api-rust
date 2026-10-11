// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Owner-only permissions for the daemon state directory and the files in it.
//!
//! The state directory (`~/.hyperdb`, or `HYPERDB_STATE_DIR`) holds the
//! daemon's connection details: `daemon.json` names the `hyperd` endpoint, and
//! `logs/` records it too. Those are the owning user's business, so this module
//! is the single place that decides how they are created.
//!
//! Created with [`std::fs::create_dir_all`] and [`std::fs::write`], both would
//! take their mode from the process umask instead — commonly `0755` and `0644`.
//! The helpers here pin the directory to `0700` and files to `0600`.
//!
//! Restricting the *directory* is the main lever on `logs/`: `hyperd` is a
//! separate process writing its own log files under its own umask, so the mode
//! it gives a freshly rotated log is not ours to choose, and a `0700` directory
//! settles the path to every file inside, whoever wrote it. Files already
//! present are tightened too, best effort — closing the directory does nothing
//! about a descriptor or hard link taken while it was still open, which is the
//! state an upgrade actually meets.
//!
//! # Platform behaviour
//!
//! Unix modes have no Windows equivalent, so the mode calls compile out there.
//! Windows leans on ACL inheritance instead: the default state directory sits
//! under `%USERPROFILE%`, whose ACL grants the owning user, `SYSTEM`, and
//! administrators — and *not* other interactive users — and a new subdirectory
//! inherits it.
//!
//! ACL inheritance is therefore the whole of the Windows protection, and it is
//! positional: a state directory outside the profile does not get it. Two
//! things can move it there, and they are not equivalent. Setting
//! `HYPERDB_STATE_DIR` is the user's own call, so it is documented as such
//! beside the variable in `README.md` rather than second-guessed here.
//! Inheriting a `HOME` from an MSYS2, Cygwin or Git Bash shell is *not* a
//! choice about this directory at all, so
//! [`super::discovery::state_dir`]'s resolution prefers `%USERPROFILE%` on
//! Windows and keeps `HOME` only as a last resort.

use std::io;
use std::path::Path;

/// Directory mode for state directories: owner-only, including traversal.
#[cfg(unix)]
const STATE_DIR_MODE: u32 = 0o700;

/// File mode for state files: readable and writable only by the owner.
const STATE_FILE_MODE: u32 = 0o600;

/// Check that `dir` is a state directory only the current user can write to.
///
/// On Unix the state directory is what keeps other users away from the control
/// socket, the lock and the discovery record, so it is checked rather than
/// assumed: it must be owned by the effective user and carry no group or other
/// write bit. [`ensure_owner_only_dir`] tightens the mode of a directory this
/// user owns, so in practice the check fails for a directory someone else owns
/// (which `chmod` cannot fix) and for a filesystem that refuses `chmod`. The
/// daemon calls this after `ensure_owner_only_dir` and refuses to start on an
/// error; a client calls it on its own, without creating or changing anything,
/// and goes straight to local mode.
///
/// A symlink to a directory is judged by its target. On Windows the default
/// location is per-user through ACL inheritance and the pipe DACL is the
/// boundary, so this always succeeds there.
///
/// # Errors
/// Returns `PermissionDenied` naming the path when the directory is not owned
/// by the current user or is writable by its group or by others, and the
/// underlying error when it cannot be inspected.
pub fn verify_state_dir_trusted(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = std::fs::metadata(dir)?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("the state directory {} is not a directory", dir.display()),
            ));
        }
        // SAFETY: `geteuid` takes no arguments, reads no memory and cannot fail.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "the state directory {} is owned by another user (uid {}); \
                     set HYPERDB_STATE_DIR to a directory you own",
                    dir.display(),
                    metadata.uid()
                ),
            ));
        }
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "the state directory {} is writable by its group or by others (mode \
                     {mode:04o}); remove the group and other write bits",
                    dir.display()
                ),
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Create `dir`, and any parents this call creates, restricted to the owning
/// user.
///
/// A directory this call creates is restricted from the start, at every level.
/// A *pre-existing* `dir` — from a release that created it under the process
/// umask — is tightened in place, as are the regular files directly inside it;
/// a pre-existing **parent** is left alone, so a caller that cares about an
/// ancestor must pass it too. Both call sites do, naming the state directory
/// and the `logs/` directory in it explicitly.
///
/// # Errors
/// Returns an error if the directory cannot be created. Failing to *tighten* an
/// existing directory or file is logged as a warning and tolerated, because
/// `chmod` can be refused for reasons unrelated to the contents — a state
/// directory on a filesystem with no Unix modes, say — and refusing to start
/// there would be a worse outcome than the permissions it was trying to fix.
pub fn ensure_owner_only_dir(dir: &Path) -> io::Result<()> {
    create_owner_only_dir(dir)?;
    restrict_existing_dir(dir);
    restrict_existing_files(dir);
    Ok(())
}

#[cfg(unix)]
fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(STATE_DIR_MODE)
        .create(dir)
}

#[cfg(not(unix))]
fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Tighten an existing directory to owner-only access, best effort.
///
/// Tolerating failure here is deliberate. `chmod` can fail for reasons that
/// have nothing to do with this directory's contents: `HYPERDB_STATE_DIR` may
/// name a path on a filesystem with no Unix modes at all (an SMB or NFS mount,
/// exFAT, some container bind-mounts), where the call is refused however the
/// directory is actually protected. Refusing to start would trade a
/// confidentiality gap the user already had for a total loss of the MCP on
/// those setups — a strictly worse outcome for a local developer tool.
///
/// The file we are about to *publish* an endpoint into is held to more than
/// this: [`write_owner_only_atomic`] tolerates the same refusal only when the
/// mode already on disk is owner-only anyway, and fails loudly when it is not.
/// The two halves have to agree, because the filesystems that refuse `chmod`
/// on a directory refuse it on the file inside as well — tolerating one and
/// hard-failing the other would leave the daemon warning about the directory
/// and then unable to publish a record at all.
fn restrict_existing_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Err(error) = restrict_existing_unix(dir, STATE_DIR_MODE) {
            tracing::warn!(
                path = %dir.display(),
                %error,
                "could not restrict the daemon state directory to the current user; \
                 its existing permissions are left in place"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Tighten the regular files directly inside `dir` to owner-only, best effort.
///
/// Restricting the directory closes the path to a *new* reader, but a file
/// already inside keeps the mode it was created with, and anything that
/// reached it before the directory was tightened — an open descriptor, a hard
/// link — still reaches it afterwards. An upgrade over a state directory an
/// earlier release created under the umask therefore has files to correct and
/// not just directories: the daemon's own log and `hyperd`'s rotated logs name
/// the endpoint, as `daemon.json` does.
///
/// One level, and regular files only. Both call sites pass `logs/` as well as
/// the state directory, so there is nothing deeper that this module puts the
/// endpoint into, and descending further would take a permission sweep into
/// whatever a user pointed `HYPERDB_STATE_DIR` at. Symlinks are skipped rather
/// than followed, since the target is not this directory's business.
///
/// Failure is tolerated exactly as in [`restrict_existing_dir`], and for the
/// same reason.
fn restrict_existing_files(dir: &Path) {
    #[cfg(unix)]
    {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    path = %dir.display(),
                    %error,
                    "could not list the daemon state directory to restrict the files in it; \
                     their existing permissions are left in place"
                );
                return;
            }
        };
        for entry in entries.flatten() {
            // From the directory read, so this reports a symlink as a symlink
            // rather than as whatever it points at.
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path();
            if let Err(error) = restrict_existing_unix(&path, STATE_FILE_MODE) {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not restrict a file in the daemon state directory to the current \
                     user; its existing permissions are left in place"
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Reset an existing path's permission bits to `mode`.
///
/// Skips the `chmod` when the mode already matches, so the steady state costs
/// one `stat`. `permissions().mode()` carries the file-type bits too, hence the
/// mask before comparing.
#[cfg(unix)]
fn restrict_existing_unix(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = std::fs::metadata(path)?;
    if metadata.permissions().mode() & 0o777 == mode {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Write `contents` to `path` atomically, restricted to the owning user.
///
/// `tmp_path` receives the content and is then renamed onto `path`, preserving
/// the existing guarantee that a concurrent reader sees either the old record
/// or the new one, never a partial write. The mode is set on `tmp_path` *before
/// any content is written*, so the endpoint is never on disk in a
/// world-readable file — not even briefly.
///
/// Because `rename` replaces the target's inode rather than its contents, the
/// published file carries this mode even if `path` already existed with looser
/// permissions. That is what corrects a `daemon.json` left behind by an earlier
/// release.
///
/// `tmp_path` is unlinked and created exclusively rather than reopened, so the
/// file the mode lands on is always one this call made — not a temp file an
/// interrupted earlier write left behind, and not another entry that happens
/// to bear the name.
///
/// # Errors
/// Returns an error if the temp file cannot be created with the intended mode,
/// cannot be written, or cannot be renamed onto `path`. Unlike a directory
/// that merely already exists, this file is one this process is about to
/// publish an endpoint into, so a record left readable beyond its owner is not
/// published at all — see [`restrict_open_state_file`] for the one refusal
/// that is tolerated, and why it does not weaken that.
pub(crate) fn write_owner_only_atomic(
    path: &Path,
    tmp_path: &Path,
    contents: &[u8],
) -> io::Result<()> {
    write_owner_only_atomic_with(path, tmp_path, contents, chmod_descriptor)
}

/// How an open state file's mode is set.
///
/// A parameter only so the mode-less-filesystem branch of
/// [`restrict_open_state_file`] can be exercised without one of the exotic
/// filesystems it exists for. Production always passes [`chmod_descriptor`].
type ChmodFn = fn(&std::fs::File, u32) -> io::Result<()>;

#[cfg(unix)]
fn chmod_descriptor(file: &std::fs::File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

// `allow` rather than `expect`: this is the only configuration in which the
// lint fires, and it is not one the Unix development host can compile, so an
// unfulfilled expectation could not be caught here.
#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the signature is fixed by `ChmodFn`, whose Unix implementation can fail; there \
              is no mode to set here, so this reports success"
)]
fn chmod_descriptor(_file: &std::fs::File, _mode: u32) -> io::Result<()> {
    Ok(())
}

fn write_owner_only_atomic_with(
    path: &Path,
    tmp_path: &Path,
    contents: &[u8],
    chmod: ChmodFn,
) -> io::Result<()> {
    use std::io::Write as _;

    // An interrupted earlier write can leave `tmp_path` behind. Unlink it
    // instead of reopening it, so the exclusive create below always gets a
    // file this call made and `OpenOptions::mode` always applies. `remove_file`
    // unlinks the name, so a leftover symlink goes rather than its target.
    match std::fs::remove_file(tmp_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = std::fs::OpenOptions::new();
    // `create_new` is `O_CREAT | O_EXCL`, which fails on an existing entry of
    // this name rather than opening it — a symlink included, since `O_EXCL`
    // refuses one even when it dangles. `O_NOFOLLOW` states the same intent
    // outright. Should something reappear at the name between the unlink and
    // the open, the open is what fails, so the record is not published rather
    // than published somewhere else.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(STATE_FILE_MODE).custom_flags(libc::O_NOFOLLOW);
    }

    let mut file = options.open(tmp_path)?;

    // `OpenOptions::mode` is a request, not a guarantee: the process umask
    // clears bits from it, and a filesystem that stores no modes ignores it.
    // Restricting the descriptor pins the result, and does so before any
    // content is written.
    restrict_open_state_file(&file, chmod)?;

    file.write_all(contents)?;
    // Close before renaming: Windows refuses to rename a file that is still
    // open for writing.
    drop(file);

    std::fs::rename(tmp_path, path)
}

/// Restrict an open state file to [`STATE_FILE_MODE`], distinguishing "the
/// mode could not be set" from "this filesystem does not have modes to set".
///
/// A filesystem whose permissions come from mount options rather than from
/// each file — an SMB or NFS mount, exFAT or vfat, some container bind-mounts
/// — refuses `chmod` outright while still reporting a mode, taken from
/// `fmask`. Propagating that refusal would leave the daemon unable to publish
/// `daemon.json` at all on those setups, which is the total loss of the MCP
/// that [`restrict_existing_dir`] declines to cause one directory earlier: the
/// same filesystem fails both calls, so warning about the directory and then
/// failing on the file would be the worst of both policies.
///
/// So a refusal is reconciled against what is actually on disk. If the
/// descriptor already reads back owner-only, the record is protected — the
/// filesystem simply reports a fixed mode — and the write proceeds. If it
/// reads back wider, nothing has protected the record, the error stands, and
/// it is not published. That keeps the invariant the loud failure existed for
/// without breaking a configuration that was already safe.
fn restrict_open_state_file(file: &std::fs::File, chmod: ChmodFn) -> io::Result<()> {
    match chmod(file, STATE_FILE_MODE) {
        Ok(()) => Ok(()),
        Err(error) => reconcile_refused_chmod(file, error),
    }
}

#[cfg(unix)]
fn reconcile_refused_chmod(file: &std::fs::File, error: io::Error) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let observed = file.metadata()?.permissions().mode() & 0o777;
    if !is_owner_only(observed) {
        return Err(error);
    }
    tracing::warn!(
        %error,
        mode = format!("{observed:04o}"),
        "could not set the mode on the daemon state file; the filesystem reports it as \
         owner-only already, so the record is published with the mode it has"
    );
    Ok(())
}

#[cfg(not(unix))]
fn reconcile_refused_chmod(_file: &std::fs::File, error: io::Error) -> io::Result<()> {
    Err(error)
}

/// Whether `mode` grants nothing to group or to other.
///
/// Only the group and other bits are consulted: owner-execute makes no
/// difference to who besides the owner can read the record, and a filesystem
/// reporting a fixed mode may well set it.
#[cfg(unix)]
#[expect(
    clippy::verbose_bit_mask,
    reason = "`trailing_zeros() >= 6` is the same test but says nothing about permissions; \
              the octal mask names the group and other bits it clears"
)]
fn is_owner_only(mode: u32) -> bool {
    mode & 0o077 == 0
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::TempDir;

    use super::*;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("fixture path should exist")
            .permissions()
            .mode()
            & 0o777
    }

    /// Stands in for a filesystem that refuses `chmod` outright but already
    /// reports the file as owner-only, the way a mount whose `fmask` denies
    /// group and other does.
    fn refuse_chmod(_file: &std::fs::File, _mode: u32) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EPERM))
    }

    /// Stands in for a filesystem that refuses `chmod` and reports a mode
    /// wider than the one asked for.
    fn refuse_chmod_over_a_wider_mode(file: &std::fs::File, _mode: u32) -> io::Result<()> {
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .expect("the fixture filesystem must support widening the temp file");
        Err(io::Error::from_raw_os_error(libc::EPERM))
    }

    #[test]
    fn state_directory_is_created_owner_only() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("state");

        ensure_owner_only_dir(&dir).unwrap();

        assert_eq!(
            mode_of(&dir),
            0o700,
            "a freshly created state directory must not be reachable by other users"
        );
    }

    #[test]
    fn nested_state_directory_is_created_owner_only_at_every_level() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("state");
        let nested = root.join("logs");

        ensure_owner_only_dir(&nested).unwrap();

        assert_eq!(
            mode_of(&nested),
            0o700,
            "the leaf state directory must be owner-only"
        );
        assert_eq!(
            mode_of(&root),
            0o700,
            "an intermediate state directory must be owner-only too, or the leaf is still \
             reachable through it"
        );
    }

    #[test]
    fn a_pre_existing_world_readable_state_directory_is_tightened() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("state");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            mode_of(&dir),
            0o755,
            "fixture must start world-readable or it does not exercise the correction"
        );

        ensure_owner_only_dir(&dir).unwrap();

        assert_eq!(
            mode_of(&dir),
            0o700,
            "a state directory left loose by an earlier run must be corrected, not accepted"
        );
    }

    /// The shape an upgrade actually meets: both the state directory and the
    /// `logs/` directory inside it already exist, world-readable, from a
    /// release that created them under the process umask.
    #[test]
    fn a_pre_existing_world_readable_directory_tree_is_tightened_at_every_level() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("state");
        let nested = root.join("logs");
        std::fs::create_dir_all(&nested).unwrap();
        for dir in [&root, &nested] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                mode_of(dir),
                0o755,
                "fixture must start world-readable or it does not exercise the correction"
            );
        }

        // Mirrors the daemon startup order: the state directory, then the log
        // directory inside it.
        ensure_owner_only_dir(&root).unwrap();
        ensure_owner_only_dir(&nested).unwrap();

        assert_eq!(
            mode_of(&root),
            0o700,
            "an existing state directory must be tightened on the next daemon start"
        );
        assert_eq!(
            mode_of(&nested),
            0o700,
            "an existing log directory must be tightened too — it holds hyperd's own logs, \
             which name the endpoint"
        );
    }

    /// Tightening the directory closes the path to a new reader, but says
    /// nothing about a file already inside it or about anything already
    /// holding that file open. The log files an earlier release left in
    /// `logs/` name the endpoint, so they are corrected too.
    #[test]
    fn pre_existing_files_in_a_loose_state_directory_are_tightened() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("logs");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let group_readable = dir.join("hyperd-0.log");
        let world_readable = dir.join("hyperdb-daemon.log");
        for (file, mode) in [(&group_readable, 0o660), (&world_readable, 0o644)] {
            std::fs::write(file, b"endpoint").unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                mode_of(file),
                mode,
                "fixture must start readable beyond its owner or it does not exercise the \
                 correction"
            );
        }

        ensure_owner_only_dir(&dir).unwrap();

        assert_eq!(mode_of(&dir), 0o700);
        for file in [&group_readable, &world_readable] {
            assert_eq!(
                mode_of(file),
                0o600,
                "a log file left readable beyond its owner must be corrected, not just \
                 covered by the directory"
            );
            assert_eq!(
                std::fs::read(file).unwrap(),
                b"endpoint",
                "tightening a log file must not disturb its contents"
            );
        }
    }

    /// The sweep stays one level deep and leaves anything that is not a
    /// regular file alone, so it cannot walk out of the state directory
    /// through a link or descend into whatever `HYPERDB_STATE_DIR` names.
    #[test]
    fn the_file_sweep_does_not_follow_links_or_descend() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("state");
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"unrelated").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link.json")).unwrap();
        let deeper = nested.join("deeper.log");
        std::fs::write(&deeper, b"deeper").unwrap();
        std::fs::set_permissions(&deeper, std::fs::Permissions::from_mode(0o644)).unwrap();

        ensure_owner_only_dir(&dir).unwrap();

        assert_eq!(
            mode_of(&outside),
            0o644,
            "the sweep must not follow a link out of the state directory"
        );
        assert_eq!(
            mode_of(&deeper),
            0o644,
            "the sweep must not descend; callers name each directory they care about"
        );
    }

    #[test]
    fn state_file_is_written_owner_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.json");
        let tmp_path = tmp.path().join("daemon.json.tmp");

        write_owner_only_atomic(&path, &tmp_path, b"{}").unwrap();

        assert_eq!(
            mode_of(&path),
            0o600,
            "a state file naming the endpoint must be readable only by its owner"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        assert!(
            !tmp_path.exists(),
            "the temp file should have been renamed onto the target"
        );
    }

    #[test]
    fn replacing_a_world_readable_state_file_tightens_it() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.json");
        let tmp_path = tmp.path().join("daemon.json.tmp");
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            mode_of(&path),
            0o644,
            "fixture must start world-readable or it does not exercise the correction"
        );

        write_owner_only_atomic(&path, &tmp_path, b"{}").unwrap();

        assert_eq!(
            mode_of(&path),
            0o600,
            "rewriting a state file left loose by an earlier release must tighten it"
        );
    }

    #[test]
    fn a_world_readable_leftover_temp_file_does_not_leak_the_new_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.json");
        let tmp_path = tmp.path().join("daemon.json.tmp");
        // An interrupted earlier write leaves the temp file behind, carrying
        // whatever mode it was created with. Writing into that file as-is
        // would put the new record in a world-readable file.
        std::fs::write(&tmp_path, b"interrupted").unwrap();
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_owner_only_atomic(&path, &tmp_path, b"{}").unwrap();

        assert_eq!(
            mode_of(&path),
            0o600,
            "a leftover temp file must not carry its loose mode into the published record"
        );
    }

    /// The temp file must be one this call created, not whatever its name
    /// already resolves to. A stale link of that name is replaced rather than
    /// followed: otherwise the record would be written wherever the link
    /// points — outside the state directory, where none of this module's
    /// permissions apply — and `daemon.json` would end up as a link instead of
    /// the record.
    #[test]
    fn a_symlinked_temp_path_is_replaced_rather_than_followed() {
        let tmp = TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let unrelated = tmp.path().join("unrelated.txt");
        std::fs::write(&unrelated, b"unrelated contents").unwrap();

        let path = state.join("daemon.json");
        let tmp_path = state.join("daemon.json.tmp");
        std::os::unix::fs::symlink(&unrelated, &tmp_path).unwrap();

        write_owner_only_atomic(&path, &tmp_path, b"{}").unwrap();

        assert_eq!(
            std::fs::read(&unrelated).unwrap(),
            b"unrelated contents",
            "a stale link at the temp path must not redirect the record out of the state directory"
        );
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_file(),
            "the published record must be a regular file, not a link left in place"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        assert_eq!(mode_of(&path), 0o600);
    }

    /// A filesystem whose modes come from mount options rather than from each
    /// file — an SMB or NFS mount, exFAT, some container bind-mounts — refuses
    /// `chmod` while still reporting a mode. When the mode it reports is
    /// already owner-only, the record is protected, and refusing to publish it
    /// would cost the user the MCP over a permission that is in fact correct.
    #[test]
    fn a_refused_chmod_still_publishes_a_record_that_is_already_owner_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.json");
        let tmp_path = tmp.path().join("daemon.json.tmp");

        write_owner_only_atomic_with(&path, &tmp_path, b"{}", refuse_chmod).expect(
            "a record the filesystem already reports as owner-only must still be published",
        );

        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
    }

    /// The converse, which is the invariant the tolerance must not cost us: a
    /// refused `chmod` over a mode that really is wider must not publish.
    #[test]
    fn a_refused_chmod_over_a_wider_mode_publishes_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.json");
        let tmp_path = tmp.path().join("daemon.json.tmp");

        let error =
            write_owner_only_atomic_with(&path, &tmp_path, b"{}", refuse_chmod_over_a_wider_mode)
                .expect_err("a record whose mode could not be restricted must not be published");

        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        assert!(
            !path.exists(),
            "no record must be published when its mode could not be restricted"
        );
    }

    #[test]
    fn trusted_check_accepts_an_owner_only_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("state");
        ensure_owner_only_dir(&dir).unwrap();
        verify_state_dir_trusted(&dir).expect("a 0700 directory we own is trusted");
    }

    #[test]
    fn trusted_check_refuses_a_group_or_other_writable_directory() {
        let tmp = TempDir::new().unwrap();
        for mode in [0o770, 0o707, 0o777, 0o720] {
            let dir = tmp.path().join(format!("state-{mode:o}"));
            ensure_owner_only_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            let error = verify_state_dir_trusted(&dir)
                .expect_err("a directory writable by group or others must be refused");
            assert_eq!(
                error.kind(),
                io::ErrorKind::PermissionDenied,
                "mode {mode:o}"
            );
            assert!(
                error.to_string().contains(&dir.display().to_string()),
                "the error must name the path: {error}"
            );
        }
    }

    #[test]
    fn trusted_check_tolerates_group_and_other_read() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("state");
        ensure_owner_only_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        verify_state_dir_trusted(&dir).expect("read and traverse for others is not a write risk");
    }

    #[test]
    fn trusted_check_refuses_a_directory_owned_by_someone_else() {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return; // root owns "/" too, so there is nothing foreign to point at
        }
        let error = verify_state_dir_trusted(Path::new("/"))
            .expect_err("the root directory is owned by root, not by this user");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("owned by another user"),
            "{error}"
        );
    }

    #[test]
    fn trusted_check_reports_a_missing_directory() {
        let tmp = TempDir::new().unwrap();
        let error = verify_state_dir_trusted(&tmp.path().join("absent")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
