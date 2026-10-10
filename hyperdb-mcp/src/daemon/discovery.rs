// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Discovery file management for the single-instance daemon.
//!
//! The daemon writes a JSON file to `<state dir>/daemon.json` (by default
//! `~/.hyperdb/daemon.json`) containing its PID, the `hyperd` endpoint and its
//! health endpoint. Clients read this file to locate the running daemon,
//! validating liveness via an identified `PING` on the health endpoint before
//! trusting it. The file is a locator, not a lock: whether a daemon is running
//! is decided by [`super::lock::DaemonLock`].

use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::control::HealthEndpoint;

const MAX_DISCOVERY_FILE_BYTES: usize = 64 * 1024;

/// Information written by the daemon so clients can discover and connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// OS process ID of the daemon.
    pub pid: u32,
    /// The `hyperd` libpq endpoint clients should connect to (e.g. `127.0.0.1:54321`).
    pub hyperd_endpoint: String,
    /// Where the daemon's health listener is bound: a Unix socket path, or a
    /// named-pipe name on Windows. Clients accept it only if it is the
    /// endpoint their own state directory implies (see
    /// [`HealthEndpoint::from_record`]).
    pub health_endpoint: String,
    /// ISO-8601 timestamp when the daemon started.
    pub started_at: String,
    /// Version of the daemon binary.
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DaemonBuildIdentity {
    mcp_version: String,
    executable_path: crate::diagnostics::ReportedPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DaemonRecord {
    #[serde(flatten)]
    info: DaemonInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<DaemonBuildIdentity>,
}

impl DaemonRecord {
    pub(super) fn with_current_identity(info: &DaemonInfo) -> io::Result<Self> {
        let executable = std::env::current_exe()?;
        Ok(Self {
            info: info.clone(),
            identity: Some(DaemonBuildIdentity {
                mcp_version: crate::version::mcp_version_string(),
                executable_path: crate::diagnostics::ReportedPath::from_os_str(
                    executable.as_os_str(),
                ),
            }),
        })
    }

    pub(crate) fn info(&self) -> &DaemonInfo {
        &self.info
    }

    pub(crate) fn identity(&self) -> Option<&DaemonBuildIdentity> {
        self.identity.as_ref()
    }
}

impl DaemonBuildIdentity {
    pub(crate) fn mcp_version(&self) -> &str {
        &self.mcp_version
    }

    pub(crate) fn executable_path(&self) -> &crate::diagnostics::ReportedPath {
        &self.executable_path
    }
}

/// Bytes carried alongside a [`RawDiscoveryRead`] classification so a caller
/// willing to try a looser shape can re-parse them without a second `read`.
///
/// `Debug` deliberately renders only the length. `RawDiscoveryRead` is
/// `Debug`-formatted into diagnostics, and a discovery file this process could
/// not parse may contain anything at all — a `Vec<u8>`'s derived `Debug` would
/// echo every one of those bytes into a doctor report or log line, just as a
/// decimal list rather than as text.
pub(crate) struct DiscoveryBytes(Vec<u8>);

impl DiscoveryBytes {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for DiscoveryBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "<{} bytes>", self.0.len())
    }
}

#[derive(Debug)]
pub(crate) enum RawDiscoveryRead {
    Missing {
        path: crate::diagnostics::ReportedPath,
    },
    Unreadable {
        path: crate::diagnostics::ReportedPath,
        kind: io::ErrorKind,
    },
    Malformed {
        path: crate::diagnostics::ReportedPath,
        /// The bytes that failed to parse, carried so a caller willing to try
        /// a looser shape (see [`discover`]) can do so without a second
        /// `read` — which would cost redundant I/O and let a rewrite land
        /// between the two parses.
        contents: DiscoveryBytes,
    },
    /// The file exceeded [`MAX_DISCOVERY_FILE_BYTES`] before JSON parsing was
    /// even attempted. Distinct from [`Self::Malformed`]: the content may be
    /// perfectly well-formed JSON — it's merely larger than any legitimate
    /// discovery record should be — so callers (the doctor) should report
    /// what actually happened instead of sending the user to fix "invalid
    /// JSON" that isn't.
    Oversized {
        path: crate::diagnostics::ReportedPath,
        /// The bytes read before the cap was hit, carried for the same reason
        /// as [`Self::Malformed`]'s.
        contents: DiscoveryBytes,
    },
    Parsed {
        path: crate::diagnostics::ReportedPath,
        record: DaemonRecord,
    },
}

pub(crate) fn read_discovery_file_raw(path: &Path) -> RawDiscoveryRead {
    let reported_path = crate::diagnostics::ReportedPath::from_os_str(path.as_os_str());
    let file = match open_discovery_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return RawDiscoveryRead::Missing {
                path: reported_path,
            };
        }
        Err(error) => {
            return RawDiscoveryRead::Unreadable {
                path: reported_path,
                kind: error.kind(),
            };
        }
    };

    let is_regular_file = match file.metadata() {
        Ok(metadata) => metadata.file_type().is_file(),
        Err(error) => {
            return RawDiscoveryRead::Unreadable {
                path: reported_path,
                kind: error.kind(),
            };
        }
    };
    if !is_regular_file {
        return RawDiscoveryRead::Unreadable {
            path: reported_path,
            kind: io::ErrorKind::InvalidInput,
        };
    }

    let mut contents = Vec::with_capacity(MAX_DISCOVERY_FILE_BYTES + 1);
    let read_limit = u64::try_from(MAX_DISCOVERY_FILE_BYTES + 1).unwrap_or(u64::MAX);
    if let Err(error) = file.take(read_limit).read_to_end(&mut contents) {
        return RawDiscoveryRead::Unreadable {
            path: reported_path,
            kind: error.kind(),
        };
    }
    if contents.len() > MAX_DISCOVERY_FILE_BYTES {
        return RawDiscoveryRead::Oversized {
            path: reported_path,
            contents: DiscoveryBytes::new(contents),
        };
    }

    parse_discovery_contents(reported_path, contents)
}

/// Read the discovery file the way the client fast path does: follow
/// symlinks and accept any valid record size (unlike the doctor's bounded,
/// no-follow [`read_discovery_file_raw`]).
fn read_discovery_file_legacy(path: &Path) -> RawDiscoveryRead {
    let reported_path = crate::diagnostics::ReportedPath::from_os_str(path.as_os_str());
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return RawDiscoveryRead::Missing {
                path: reported_path,
            };
        }
        Err(error) => {
            return RawDiscoveryRead::Unreadable {
                path: reported_path,
                kind: error.kind(),
            };
        }
    };
    parse_discovery_contents(reported_path, contents)
}

fn parse_discovery_contents(
    reported_path: crate::diagnostics::ReportedPath,
    contents: Vec<u8>,
) -> RawDiscoveryRead {
    match serde_json::from_slice(&contents) {
        Ok(record) => RawDiscoveryRead::Parsed {
            path: reported_path,
            record,
        },
        Err(_) => RawDiscoveryRead::Malformed {
            path: reported_path,
            contents: DiscoveryBytes::new(contents),
        },
    }
}

fn open_discovery_file(path: &Path) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        // `O_NONBLOCK` makes FIFO/device rejection prompt, while `O_NOFOLLOW`
        // prevents a symlink swap from turning the checked input into a
        // blocking special file between path inspection and open.
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "daemon discovery source is not a regular file",
            ));
        }
        std::fs::File::open(path)
    }
}

/// Returns the directory used for daemon state files.
///
/// Resolution order:
/// 1. `HYPERDB_STATE_DIR` environment variable (if set)
/// 2. `~/.hyperdb/` (where `~` is `HOME` on Unix, `USERPROFILE` on Windows)
///
/// # Errors
/// Returns an error if neither the env var nor the home directory can be determined.
pub fn state_dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("HYPERDB_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = home_dir().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "cannot determine home directory")
    })?;
    Ok(home.join(".hyperdb"))
}

/// Returns the path to the discovery file.
///
/// # Errors
/// Returns an error if the home directory cannot be determined.
pub fn discovery_file_path() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("daemon.json"))
}

/// Write the discovery file atomically (write-to-temp then rename).
///
/// # Errors
/// Returns an error if the state directory cannot be created or the file cannot be written.
pub fn write_discovery_file(info: &DaemonInfo) -> io::Result<()> {
    write_discovery_record(&state_dir()?, info)
}

/// [`write_discovery_file`] for an explicit state directory.
///
/// # Errors
/// As [`write_discovery_file`].
pub fn write_discovery_file_in(dir: &Path, info: &DaemonInfo) -> io::Result<()> {
    write_discovery_record(dir, info)
}

/// Write the record with this process's identity, into `dir`. The daemon
/// passes the state directory it already holds rather than re-reading the
/// environment.
pub(super) fn write_enriched_discovery_file_in(dir: &Path, info: &DaemonInfo) -> io::Result<()> {
    let record = DaemonRecord::with_current_identity(info)?;
    write_discovery_record(dir, &record)
}

fn write_discovery_record(dir: &Path, record: &(impl Serialize + ?Sized)) -> io::Result<()> {
    super::state_perms::ensure_owner_only_dir(dir)?;

    let path = dir.join("daemon.json");
    let tmp_path = dir.join("daemon.json.tmp");
    let json = serde_json::to_string_pretty(record).map_err(|e| io::Error::other(e.to_string()))?;
    // Writes into `tmp_path` and renames it onto `path`. `std::fs::rename`
    // already replaces an existing target atomically on both Unix
    // (`rename(2)`) and Windows (`MoveFileExW` with
    // `MOVEFILE_REPLACE_EXISTING`, falling back to `SetFileInformationByHandle`
    // with `FILE_RENAME_FLAG_REPLACE_IF_EXISTS`). Pre-deleting the target
    // would reintroduce exactly the window this function's doc comment
    // promises not to have: a concurrent `discover()` could observe the file
    // as `Missing` mid-restart (see `try_restart_hyperd`, which rewrites this
    // file on every `hyperd` restart).
    //
    // The rename is also what tightens a record an earlier release left
    // world-readable, because it replaces the target's inode rather than
    // rewriting it in place.
    super::state_perms::write_owner_only_atomic(&path, &tmp_path, json.as_bytes())
}

/// Read the discovery file and validate that the daemon is still alive.
/// Returns `None` if no daemon is running (file missing, state directory not
/// trusted, health endpoint not the one this state directory implies, or the
/// daemon does not answer an identified `PING`).
///
/// Side-effect free: a stale record is left for `remove_stale_record`, which
/// runs under the daemon lock where it cannot race a starting daemon.
pub fn discover() -> Option<DaemonInfo> {
    discover_in(&state_dir().ok()?)
}

/// [`discover`] for an explicit state directory.
pub fn discover_in(dir: &Path) -> Option<DaemonInfo> {
    // A state directory another user can write to could hold a record that
    // points this client at an endpoint of someone else's choosing.
    match super::state_perms::verify_state_dir_trusted(dir) {
        Ok(()) => {}
        Err(error) => {
            tracing::debug!(kind = ?error.kind(), "daemon state directory is not usable for discovery");
            return None;
        }
    }
    let discovery_path = dir.join("daemon.json");
    // Preserve the historical client-discovery contract: normal discovery
    // follows symlinks and accepts any valid record size. Doctor uses the
    // separate bounded, no-follow raw reader above because it must never
    // mutate or block on a special file.
    //
    // The `RawDiscoveryRead` classification below chooses a debug-log message
    // and short-circuits a missing/unreadable file. It must never gate whether
    // a *live* daemon is discovered: an unrecognized, reshaped, or absent
    // `identity` block is forward-compatible noise to this fast path (see
    // docs/superpowers/specs/2026-08-13-hyperdb-mcp-agent-ux-design.md,
    // "Unknown fields remain forward compatible"). So when the strict
    // `DaemonRecord` parse fails, re-parse the bytes the classification already
    // carries as a tolerant `DaemonInfo`, which ignores unknown fields.
    let info = match read_discovery_file_legacy(&discovery_path) {
        RawDiscoveryRead::Missing { path } => {
            tracing::debug!(encoding = ?path.encoding, "daemon discovery file is missing");
            return None;
        }
        RawDiscoveryRead::Unreadable { path, kind } => {
            tracing::debug!(?kind, encoding = ?path.encoding, "daemon discovery file is unreadable");
            return None;
        }
        // `read_discovery_file_legacy` applies no size cap, so it does not
        // produce `Oversized` today. Folding it in with `Malformed` keeps that
        // from mattering.
        RawDiscoveryRead::Oversized { path, contents }
        | RawDiscoveryRead::Malformed { path, contents } => {
            let Ok(info) = serde_json::from_slice::<DaemonInfo>(contents.as_slice()) else {
                tracing::debug!(encoding = ?path.encoding, "daemon discovery file is malformed");
                return None;
            };
            tracing::debug!(encoding = ?path.encoding, "daemon discovery file failed the strict parse; accepted as DaemonInfo");
            info
        }
        RawDiscoveryRead::Parsed { path, record } => {
            tracing::debug!(encoding = ?path.encoding, "daemon discovery file parsed (enriched)");
            record.info().clone()
        }
    };

    let endpoint = HealthEndpoint::from_record(&info.health_endpoint, dir)?;
    if is_daemon_alive(&endpoint) {
        Some(info)
    } else {
        None
    }
}

/// Delete a daemon record that a dead daemon left behind.
///
/// The caller must hold the [`DaemonLock`](super::lock::DaemonLock) for
/// `dir`: a held lock proves no daemon of this shape is running, and holding
/// it keeps one from starting while the record is judged. Only a record of the
/// current shape (it carries `health_endpoint`) is removed. A record in
/// another shape may belong to a daemon from a release that does not take the
/// lock, and is left alone.
pub(super) fn remove_stale_record(dir: &Path) {
    let path = dir.join("daemon.json");
    // `Parsed` requires `health_endpoint`, which no earlier shape carries.
    if matches!(
        read_discovery_file_raw(&path),
        RawDiscoveryRead::Parsed { .. }
    ) {
        let _ = std::fs::remove_file(&path);
    }
}

/// The record in `dir`, if it is a pre-1.0 (legacy-shaped) one that names a
/// TCP health port. Reads like doctor does: bounded, and without following a
/// symlink. A current-shape record yields `None`.
pub(super) fn read_legacy_record(dir: &Path) -> Option<super::legacy::LegacyRecord> {
    match read_discovery_file_raw(&dir.join("daemon.json")) {
        RawDiscoveryRead::Malformed { contents, .. } => {
            super::legacy::LegacyRecord::parse(contents.as_slice())
        }
        _ => None,
    }
}

/// A sentence for the CLI when `dir` holds a pre-1.0 daemon's record: that
/// daemon holds no daemon lock and speaks TCP, so `daemon stop` and `status`
/// cannot reach it. `None` when there is no legacy record.
#[must_use]
pub fn legacy_daemon_hint(dir: &Path) -> Option<String> {
    let record = read_legacy_record(dir)?;
    Some(format!(
        "{} records a pre-1.0 daemon (pid {}, port {}), which this version cannot \
         stop or inspect. Starting a hyperdb-mcp session retires it automatically; \
         or stop it yourself (kill {}) and delete that file.",
        dir.join("daemon.json").display(),
        record.pid,
        record.health_port,
        record.pid
    ))
}

/// Remove the legacy record `expected`, if `dir` still holds exactly it.
///
/// The caller must hold the [`DaemonLock`](super::lock::DaemonLock) for `dir`.
/// The re-read keeps a record that changed since it was judged.
pub(super) fn remove_legacy_record(dir: &Path, expected: super::legacy::LegacyRecord) {
    if read_legacy_record(dir) == Some(expected) {
        let _ = std::fs::remove_file(dir.join("daemon.json"));
    }
}

/// Remove the discovery file (called during graceful shutdown).
pub fn remove_discovery_file() {
    if let Ok(dir) = state_dir() {
        remove_discovery_file_in(&dir);
    }
}

/// [`remove_discovery_file`] for an explicit state directory.
pub fn remove_discovery_file_in(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("daemon.json"));
}

/// Check if the daemon is alive by sending PING and verifying the identifying
/// token. Connecting alone is not enough: any program can hold a socket.
fn is_daemon_alive(endpoint: &HealthEndpoint) -> bool {
    super::health::ping_identified(
        endpoint,
        Duration::from_millis(300),
        Duration::from_millis(300),
    )
    .is_some()
}

/// Cross-platform home directory resolution.
///
/// On Windows the user profile is consulted first. MSYS2, Cygwin and Git Bash
/// commonly set `HOME` to a path of their own outside `%USERPROFILE%`, and
/// preferring it would put the state directory outside the profile — losing the
/// inherited ACL that is the whole of the Windows protection for these files
/// (see [`super::state_perms`]) without the user having asked for it. `HOME`
/// stays as a last resort there, so a machine that resolved before still
/// resolves. This is also the order the documentation above already described,
/// and it matches `crate::paths::persistent_home_dir`.
fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("USERPROFILE")
            .filter(|profile| !profile.is_empty())
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use crate::daemon::health::{DaemonState, HealthListener};
    use crate::diagnostics::ReportedPath;

    use super::*;

    fn legacy_info() -> DaemonInfo {
        DaemonInfo {
            pid: 4242,
            hyperd_endpoint: "127.0.0.1:54321".to_string(),
            health_endpoint: "/run/hyperdb/daemon.sock".to_string(),
            started_at: "2026-08-13T12:34:56Z".to_string(),
            version: "0.7.0".to_string(),
        }
    }

    fn catch_serde<T>(operation: impl FnOnce() -> serde_json::Result<T>) -> Result<T, String> {
        catch_unwind(AssertUnwindSafe(operation))
            .map_err(|_| "operation panicked".to_string())?
            .map_err(|error| error.to_string())
    }

    fn catch_raw_read(path: &Path) -> Result<RawDiscoveryRead, String> {
        catch_unwind(AssertUnwindSafe(|| read_discovery_file_raw(path)))
            .map_err(|_| "raw discovery read panicked".to_string())
    }

    fn directory_entries(path: &Path) -> Vec<OsString> {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    fn run_discovery_compatibility_child(test_name: &str, child_sentinel_env: &str) {
        use std::process::{Command, Stdio};
        use std::time::Instant;

        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let child_marker = tmp.path().join("child-started");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env(child_sentinel_env, &child_marker)
            .env("HYPERDB_STATE_DIR", &state_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let timed_out = loop {
            match child.try_wait() {
                Ok(Some(_)) => break false,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => break true,
                Err(error) => {
                    let _ = child.kill();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "discovery compatibility child status failed: {error}\nstdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
        };

        if timed_out {
            let kill_error = child.kill().err();
            let output = child.wait_with_output().unwrap();
            panic!(
                "discovery compatibility child exceeded 5s and was killed ({kill_error:?})\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let output = child.wait_with_output().unwrap();
        assert!(
            child_marker.is_file(),
            "exact discovery compatibility child branch did not start\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "discovery compatibility child failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn daemon_record_old_and_new_flat_wire_contract() {
        let old_wire = json!({
            "pid": 4242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_endpoint": "/run/hyperdb/daemon.sock",
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0"
        });
        let identity = DaemonBuildIdentity {
            mcp_version: "0.7.0.rabc123".to_string(),
            executable_path: ReportedPath::from_os_str(OsStr::new("/opt/hyperdb/bin/hyperdb-mcp")),
        };
        let expected_new_wire = json!({
            "pid": 4242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_endpoint": "/run/hyperdb/daemon.sock",
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0",
            "identity": {
                "mcp_version": "0.7.0.rabc123",
                "executable_path": {
                    "display": "/opt/hyperdb/bin/hyperdb-mcp",
                    "encoding": "utf8"
                }
            }
        });
        let mut failures = Vec::new();

        match catch_serde(|| serde_json::from_value::<DaemonRecord>(old_wire.clone())) {
            Ok(record) => {
                if record.info != legacy_info() {
                    failures
                        .push("old flat JSON did not preserve legacy daemon fields".to_string());
                }
                if record.identity.is_some() {
                    failures
                        .push("old flat JSON should deserialize with absent identity".to_string());
                }
                match catch_serde(|| serde_json::to_value(&record)) {
                    Ok(round_trip) if round_trip == old_wire => {}
                    Ok(round_trip) => failures.push(format!(
                        "old record did not reserialize to the exact flat wire: {round_trip}"
                    )),
                    Err(error) => failures.push(format!(
                        "old record could not be reserialized after parsing: {error}"
                    )),
                }
            }
            Err(error) => failures.push(format!("old flat JSON did not deserialize: {error}")),
        }

        let new_record = DaemonRecord {
            info: legacy_info(),
            identity: Some(identity.clone()),
        };
        match catch_serde(|| serde_json::to_value(&new_record)) {
            Ok(new_wire) => {
                if new_wire != expected_new_wire {
                    failures.push(format!(
                        "new record wire was not the exact additive flat shape: {new_wire}"
                    ));
                }
                if new_wire.get("info").is_some() {
                    failures.push("new wire nested legacy fields under `info`".to_string());
                }

                match catch_serde(|| serde_json::from_value::<DaemonRecord>(new_wire.clone())) {
                    Ok(round_trip) => {
                        if round_trip.info != legacy_info()
                            || round_trip.identity.as_ref() != Some(&identity)
                        {
                            failures.push(
                                "new build/executable identity did not round-trip".to_string(),
                            );
                        }
                    }
                    Err(error) => failures.push(format!(
                        "new build/executable identity could not be deserialized: {error}"
                    )),
                }

                match serde_json::from_value::<DaemonInfo>(new_wire) {
                    Ok(old_reader) if old_reader == legacy_info() => {}
                    Ok(old_reader) => failures.push(format!(
                        "old DaemonInfo reader changed legacy fields: {old_reader:?}"
                    )),
                    Err(error) => failures.push(format!(
                        "old DaemonInfo reader rejected additive identity: {error}"
                    )),
                }
            }
            Err(error) => failures.push(format!("new record could not be serialized: {error}")),
        }

        assert!(
            failures.is_empty(),
            "daemon record wire contract failures:\n{}",
            failures.join("\n")
        );
    }

    /// A live daemon: a real `HealthListener` on a state directory's endpoint.
    struct LiveDaemon {
        state: Arc<DaemonState>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl LiveDaemon {
        fn start(dir: &Path, info: &DaemonInfo) -> Self {
            // Serve exactly the endpoint the record names: deriving a second
            // endpoint here would mint a different random pipe name on Windows.
            let endpoint = HealthEndpoint::from_record(&info.health_endpoint, dir)
                .expect("record names the endpoint of its own state directory");
            let listener = HealthListener::bind(&endpoint).unwrap();
            let state = Arc::new(DaemonState::new(endpoint));
            let info = Arc::new(Mutex::new(info.clone()));
            let run_state = Arc::clone(&state);
            let thread = std::thread::spawn(move || listener.run(run_state, info));
            Self {
                state,
                thread: Some(thread),
            }
        }

        fn stop(&mut self) {
            self.state.request_shutdown();
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    impl Drop for LiveDaemon {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// A per-test state directory short enough for a Unix socket path.
    fn short_state_dir() -> TempDir {
        TempDir::new().unwrap()
    }

    /// Info whose `health_endpoint` is the endpoint `dir` implies.
    fn info_for(dir: &Path, pid: u32) -> DaemonInfo {
        DaemonInfo {
            pid,
            health_endpoint: HealthEndpoint::for_new_daemon(dir)
                .unwrap()
                .as_str()
                .to_string(),
            ..legacy_info()
        }
    }

    fn wire_of(info: &DaemonInfo, identity: &Value) -> Value {
        json!({
            "pid": info.pid,
            "hyperd_endpoint": info.hyperd_endpoint,
            "health_endpoint": info.health_endpoint,
            "started_at": info.started_at,
            "version": info.version,
            "identity": identity,
        })
    }

    #[test]
    fn discover_tolerates_forward_incompatible_identity_shapes() {
        // Three real-world shapes a client fast path must still discover a
        // live daemon through, per the forward-compatibility contract in
        // docs/superpowers/specs/2026-08-13-hyperdb-mcp-agent-ux-design.md
        // ("Unknown fields remain forward compatible"). None of these is
        // valid input for `DaemonBuildIdentity`, so a strict `DaemonRecord`
        // parse must fail for every one of them (fixture sanity check).
        let shapes: [(&str, Value); 3] = [
            ("identity retyped as a string", json!("not-an-object")),
            (
                "executable_path reshaped",
                json!({
                    "mcp_version": "0.7.0.rabc123",
                    "executable_path": "/opt/hyperdb/bin/hyperdb-mcp"
                }),
            ),
            (
                "identity missing executable_path",
                json!({ "mcp_version": "0.7.0.rabc123" }),
            ),
        ];

        let mut failures = Vec::new();
        for (index, (label, identity_json)) in shapes.into_iter().enumerate() {
            let tmp = short_state_dir();
            let pid = 9_000 + u32::try_from(index).expect("fixture index fits in u32");
            let info = info_for(tmp.path(), pid);
            let wire = wire_of(&info, &identity_json);

            if serde_json::from_value::<DaemonRecord>(wire.clone()).is_ok() {
                failures.push(format!(
                    "{label}: fixture unexpectedly satisfied the strict DaemonRecord parse; \
                     it no longer exercises the forward-compatibility regression"
                ));
            }
            std::fs::write(
                tmp.path().join("daemon.json"),
                serde_json::to_vec(&wire).unwrap(),
            )
            .unwrap();

            let mut daemon = LiveDaemon::start(tmp.path(), &info);
            match discover_in(tmp.path()) {
                Some(discovered) if discovered == info => {}
                Some(other) => failures.push(format!(
                    "{label}: discover_in returned different facts than the live daemon: {other:?}"
                )),
                None => failures.push(format!(
                    "{label}: discover_in returned None for a live, healthy daemon whose only \
                     defect is an unrecognized `identity` block"
                )),
            }
            daemon.stop();
        }

        assert!(
            failures.is_empty(),
            "identity forward-compatibility discover failures:\n{}",
            failures.join("\n")
        );
    }

    /// `discover_in` never mutates: a stale record (nothing listening) is left
    /// in place for `remove_stale_record`, which runs under the daemon lock.
    #[test]
    fn discover_in_leaves_a_stale_record_alone() {
        let tmp = short_state_dir();
        let info = info_for(tmp.path(), 9_101);
        write_record_for_test(tmp.path(), &info);
        let path = tmp.path().join("daemon.json");
        let before = std::fs::read(&path).unwrap();

        assert!(discover_in(tmp.path()).is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    fn write_record_for_test(dir: &Path, info: &DaemonInfo) {
        std::fs::write(
            dir.join("daemon.json"),
            serde_json::to_vec_pretty(info).unwrap(),
        )
        .unwrap();
    }

    /// A record only the lenient `DaemonInfo` fallback can read must survive
    /// stale cleanup (it may belong to a newer daemon), while a current-shape
    /// record is removed: the guard is narrow, not a blanket disable.
    #[test]
    fn remove_stale_record_preserves_a_leniently_parsed_record() {
        let tmp = short_state_dir();
        let path = tmp.path().join("daemon.json");
        let info = info_for(tmp.path(), 9_100);
        let lenient_wire = wire_of(&info, &json!("not-an-object"));
        assert!(serde_json::from_value::<DaemonRecord>(lenient_wire.clone()).is_err());
        assert!(serde_json::from_value::<DaemonInfo>(lenient_wire.clone()).is_ok());
        let lenient_bytes = serde_json::to_vec(&lenient_wire).unwrap();
        std::fs::write(&path, &lenient_bytes).unwrap();

        assert!(discover_in(tmp.path()).is_none());
        remove_stale_record(tmp.path());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            lenient_bytes,
            "a record this client could only read leniently must not be deleted"
        );

        write_record_for_test(tmp.path(), &info);
        remove_stale_record(tmp.path());
        assert!(!path.exists(), "a strict current-shape record is stale");
    }

    /// A record in a shape without `health_endpoint` (an earlier release) is
    /// not this build's to delete.
    #[test]
    fn remove_stale_record_leaves_a_legacy_shaped_record() {
        let tmp = short_state_dir();
        let path = tmp.path().join("daemon.json");
        let bytes = serde_json::to_vec(&json!({
            "pid": 4242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_port": 7485,
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0"
        }))
        .unwrap();
        std::fs::write(&path, &bytes).unwrap();

        assert!(discover_in(tmp.path()).is_none());
        remove_stale_record(tmp.path());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    /// A record pointing at an endpoint this state directory does not imply is
    /// never connected to, even if something answers there. Unix pins the
    /// exact socket path; the Windows twin is below.
    #[cfg(unix)]
    #[test]
    fn discover_in_rejects_a_foreign_endpoint() {
        let live_dir = short_state_dir();
        let other_dir = short_state_dir();
        let live_info = info_for(live_dir.path(), 9_200);
        let _daemon = LiveDaemon::start(live_dir.path(), &live_info);

        // `other_dir`'s record names `live_dir`'s live socket.
        write_record_for_test(other_dir.path(), &live_info);
        assert!(discover_in(other_dir.path()).is_none());
        // And the live daemon is found through its own directory.
        write_record_for_test(live_dir.path(), &live_info);
        assert_eq!(discover_in(live_dir.path()), Some(live_info));
    }

    /// Windows: a pipe name does not encode the state directory, so only the
    /// prefix and character set are checked. A record naming another
    /// program's pipe is never connected to, even if it answers.
    #[cfg(windows)]
    #[test]
    fn discover_in_rejects_a_foreign_endpoint() {
        let live_dir = short_state_dir();
        let other_dir = short_state_dir();
        let live_info = info_for(live_dir.path(), 9_200);
        let _daemon = LiveDaemon::start(live_dir.path(), &live_info);

        for foreign in [
            r"\\.\pipe\docker_engine",
            r"\\.\pipe\hyperdb-mcp-",
            "127.0.0.1:7485",
        ] {
            let mut info = live_info.clone();
            info.health_endpoint = foreign.to_string();
            write_record_for_test(other_dir.path(), &info);
            assert!(discover_in(other_dir.path()).is_none(), "{foreign}");
        }
        // The daemon is found through a record that names its own pipe.
        write_record_for_test(live_dir.path(), &live_info);
        assert_eq!(discover_in(live_dir.path()), Some(live_info));
    }

    /// A state directory another account can write to is not trusted.
    #[cfg(unix)]
    #[test]
    fn discover_in_ignores_an_untrusted_state_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = short_state_dir();
        let info = info_for(tmp.path(), 9_300);
        let mut daemon = LiveDaemon::start(tmp.path(), &info);
        write_record_for_test(tmp.path(), &info);
        assert_eq!(discover_in(tmp.path()), Some(info));

        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(
            discover_in(tmp.path()).is_none(),
            "a group-writable state directory must not be trusted"
        );
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        daemon.stop();
    }

    #[test]
    fn raw_discovery_read_is_non_mutating_and_distinguishes_io() {
        const SECRET_SENTINEL: &str = "RAW_DISCOVERY_SECRET_MUST_NOT_LEAK";

        let tmp = TempDir::new().unwrap();
        let missing_path = tmp.path().join("missing.json");
        let unreadable_path = tmp.path().join("directory-not-file");
        std::fs::create_dir(&unreadable_path).unwrap();

        let malformed_path = tmp.path().join("malformed.json");
        let malformed_bytes = format!("{{\"secret\":\"{SECRET_SENTINEL}\"").into_bytes();
        std::fs::write(&malformed_path, &malformed_bytes).unwrap();

        let parsed_path = tmp.path().join("parsed.json");
        let parsed_bytes = serde_json::to_vec(&json!({
            "pid": 4242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_endpoint": "/run/hyperdb/daemon.sock",
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0"
        }))
        .unwrap();
        std::fs::write(&parsed_path, &parsed_bytes).unwrap();

        let entries_before = directory_entries(tmp.path());
        let mut failures = Vec::new();

        match catch_raw_read(&missing_path) {
            Ok(RawDiscoveryRead::Missing { path })
                if path == ReportedPath::from_os_str(missing_path.as_os_str()) => {}
            Ok(other) => failures.push(format!(
                "missing path was not reported as Missing with its ReportedPath: {other:?}"
            )),
            Err(error) => failures.push(format!("missing path read failed: {error}")),
        }

        match catch_raw_read(&unreadable_path) {
            Ok(RawDiscoveryRead::Unreadable { path, kind }) => {
                if path != ReportedPath::from_os_str(unreadable_path.as_os_str()) {
                    failures.push("unreadable path did not use ReportedPath".to_string());
                }
                if kind == io::ErrorKind::NotFound {
                    failures.push("non-NotFound I/O was misclassified as missing".to_string());
                }
            }
            Ok(other) => failures.push(format!(
                "directory read error was not distinguished as Unreadable: {other:?}"
            )),
            Err(error) => failures.push(format!("unreadable path read failed: {error}")),
        }

        match catch_raw_read(&malformed_path) {
            Ok(state) => {
                let rendered = format!("{state:?}");
                if rendered.contains(SECRET_SENTINEL) {
                    failures.push("malformed state leaked discovery contents".to_string());
                }
                // `Malformed` carries the unparsed bytes so `discover()` can
                // retry them without a second `read`. A derived `Debug` on a
                // `Vec<u8>` would print every byte as a decimal list — the same
                // leak, merely unsearchable — so check for that form too.
                let sentinel_as_debug_bytes = SECRET_SENTINEL
                    .as_bytes()
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                if rendered.contains(&sentinel_as_debug_bytes) {
                    failures.push(
                        "malformed state leaked discovery contents as a debug byte list"
                            .to_string(),
                    );
                }
                match state {
                    RawDiscoveryRead::Malformed { path, .. }
                        if path == ReportedPath::from_os_str(malformed_path.as_os_str()) => {}
                    other => failures.push(format!(
                        "malformed JSON was not reported as Malformed with its ReportedPath: {other:?}"
                    )),
                }
            }
            Err(error) => failures.push(format!("malformed path read failed: {error}")),
        }

        match catch_raw_read(&parsed_path) {
            Ok(RawDiscoveryRead::Parsed { path, record }) => {
                if path != ReportedPath::from_os_str(parsed_path.as_os_str()) {
                    failures.push("parsed path did not use ReportedPath".to_string());
                }
                if record.info != legacy_info() || record.identity.is_some() {
                    failures.push(
                        "old flat discovery JSON did not parse as a legacy record".to_string(),
                    );
                }
            }
            Ok(other) => failures.push(format!(
                "valid old discovery JSON was not reported as Parsed: {other:?}"
            )),
            Err(error) => failures.push(format!("parsed path read failed: {error}")),
        }

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            use crate::diagnostics::PathEncoding;

            let non_utf8_path = tmp
                .path()
                .join(OsString::from_vec(b"missing-\xff.json".to_vec()));
            match catch_raw_read(&non_utf8_path) {
                Ok(RawDiscoveryRead::Missing { path }) if path.encoding == PathEncoding::Lossy => {}
                Ok(other) => failures.push(format!(
                    "non-UTF-8 path was not safely reported as lossy Missing: {other:?}"
                )),
                Err(error) => failures.push(format!("non-UTF-8 path read failed: {error}")),
            }
        }

        if missing_path.exists() {
            failures.push("raw read created the missing discovery path".to_string());
        }
        if !unreadable_path.is_dir() {
            failures.push("raw read removed or replaced the unreadable path".to_string());
        }
        match std::fs::read(&malformed_path) {
            Ok(bytes) if bytes == malformed_bytes => {}
            _ => failures.push("raw read changed or deleted malformed discovery bytes".to_string()),
        }
        match std::fs::read(&parsed_path) {
            Ok(bytes) if bytes == parsed_bytes => {}
            _ => failures.push("raw read changed or deleted parsed discovery bytes".to_string()),
        }
        if directory_entries(tmp.path()) != entries_before {
            failures.push("raw read changed the discovery directory entries".to_string());
        }

        assert!(
            failures.is_empty(),
            "raw discovery read contract failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn raw_discovery_reports_oversized_valid_json_as_oversized() {
        const MAX_EXPECTED_DISCOVERY_BYTES: usize = 64 * 1024;

        let tmp = TempDir::new().unwrap();
        let base_bytes = serde_json::to_vec(&json!({
            "pid": 4242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_endpoint": "/run/hyperdb/daemon.sock",
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0",
            "ignored_padding": ""
        }))
        .unwrap();
        let marker = b"\"ignored_padding\":\"\"";
        let marker_start = base_bytes
            .windows(marker.len())
            .position(|window| window == marker)
            .unwrap();
        let padding_offset = marker_start + marker.len() - 1;
        let sized_fixture = |target_len: usize| {
            let mut bytes = base_bytes.clone();
            bytes.splice(
                padding_offset..padding_offset,
                vec![b'x'; target_len - bytes.len()],
            );
            bytes
        };
        let limit_bytes = sized_fixture(MAX_EXPECTED_DISCOVERY_BYTES);
        let oversized_bytes = sized_fixture(MAX_EXPECTED_DISCOVERY_BYTES + 1);
        let limit_path = tmp.path().join("limit-daemon.json");
        let oversized_path = tmp.path().join("oversized-daemon.json");
        std::fs::write(&limit_path, &limit_bytes).unwrap();
        std::fs::write(&oversized_path, &oversized_bytes).unwrap();

        let mut failures = Vec::new();
        if serde_json::from_slice::<DaemonRecord>(&limit_bytes).is_err()
            || serde_json::from_slice::<DaemonRecord>(&oversized_bytes).is_err()
        {
            failures.push("fixed-limit fixtures were not independently valid JSON".to_string());
        }
        match read_discovery_file_raw(&limit_path) {
            RawDiscoveryRead::Parsed { path: reported, .. }
                if reported == ReportedPath::from_os_str(limit_path.as_os_str()) => {}
            other => failures.push(format!(
                "valid JSON at the exact fixed limit was not accepted: {other:?}"
            )),
        }
        match read_discovery_file_raw(&oversized_path) {
            RawDiscoveryRead::Oversized { path: reported, .. }
                if reported == ReportedPath::from_os_str(oversized_path.as_os_str()) => {}
            other => failures.push(format!(
                "oversized valid JSON was not honestly classified as Oversized (not Malformed): {other:?}"
            )),
        }
        for (label, path, expected) in [
            ("limit", &limit_path, &limit_bytes),
            ("oversized", &oversized_path, &oversized_bytes),
        ] {
            match std::fs::read(path) {
                Ok(after) if after == *expected => {}
                Ok(_) => failures.push(format!("{label} discovery bytes were modified")),
                Err(error) => failures.push(format!(
                    "{label} discovery file disappeared after raw read: {error}"
                )),
            }
        }

        assert!(
            failures.is_empty(),
            "oversized raw discovery failures:\n{}",
            failures.join("\n")
        );
    }

    /// A valid record larger than the doctor's raw-read cap is still a live
    /// daemon for the client fast path, and is never rewritten by it.
    #[test]
    fn discover_in_accepts_a_live_oversized_record() {
        const RAW_DOCTOR_LIMIT_BYTES: usize = 64 * 1024;

        let tmp = short_state_dir();
        let mut oversized = info_for(tmp.path(), 5_252);
        oversized.version = "v".repeat(RAW_DOCTOR_LIMIT_BYTES + 1);
        let path = tmp.path().join("daemon.json");
        write_record_for_test(tmp.path(), &oversized);
        let original = std::fs::read(&path).unwrap();
        assert!(original.len() > RAW_DOCTOR_LIMIT_BYTES);

        assert!(matches!(
            read_discovery_file_raw(&path),
            RawDiscoveryRead::Oversized { .. }
        ));
        let mut daemon = LiveDaemon::start(tmp.path(), &oversized);
        assert_eq!(discover_in(tmp.path()), Some(oversized));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        daemon.stop();
        assert!(discover_in(tmp.path()).is_none());
        // Oversized records are not the current-shape fast path: left alone.
        remove_stale_record(tmp.path());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    /// The client fast path follows a discovery symlink; the doctor reader
    /// rejects it without touching either the link or its target.
    #[cfg(unix)]
    #[test]
    fn discover_in_follows_a_live_discovery_symlink() {
        let tmp = short_state_dir();
        let linked = info_for(tmp.path(), 6_363);
        write_record_for_test(tmp.path(), &linked);
        let link_path = tmp.path().join("daemon.json");
        let target_path = tmp.path().join("legacy-daemon-target.json");
        std::fs::rename(&link_path, &target_path).unwrap();
        std::os::unix::fs::symlink(&target_path, &link_path).unwrap();
        let target_bytes = std::fs::read(&target_path).unwrap();

        match read_discovery_file_raw(&link_path) {
            RawDiscoveryRead::Unreadable {
                path: reported,
                kind,
            } if reported == ReportedPath::from_os_str(link_path.as_os_str())
                && kind != io::ErrorKind::NotFound => {}
            other => panic!("doctor reader did not reject the symlink: {other:?}"),
        }

        let mut daemon = LiveDaemon::start(tmp.path(), &linked);
        assert_eq!(discover_in(tmp.path()), Some(linked));
        assert!(
            std::fs::symlink_metadata(&link_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        daemon.stop();
        assert!(discover_in(tmp.path()).is_none());
        // Cleanup refuses to follow or remove a symlinked record.
        remove_stale_record(tmp.path());
        assert!(
            std::fs::symlink_metadata(&link_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn raw_discovery_rejects_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::FileTypeExt as _;
        use std::path::PathBuf;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const CHILD_PATH_ENV: &str = "HYPERDB_MCP_RAW_DISCOVERY_FIFO_CHILD";
        const CHILD_MARKER_ENV: &str = "HYPERDB_MCP_RAW_DISCOVERY_FIFO_MARKER";
        const TEST_NAME: &str =
            "daemon::discovery::tests::raw_discovery_rejects_fifo_without_blocking";

        if let Some(path) = std::env::var_os(CHILD_PATH_ENV) {
            let path = PathBuf::from(path);
            let marker = PathBuf::from(
                std::env::var_os(CHILD_MARKER_ENV)
                    .expect("FIFO child marker path must accompany child path"),
            );
            std::fs::write(marker, b"started").unwrap();
            let mut failures = Vec::new();
            match read_discovery_file_raw(&path) {
                RawDiscoveryRead::Unreadable { kind, .. } if kind != io::ErrorKind::NotFound => {}
                other => failures.push(format!(
                    "FIFO was not rejected as a non-NotFound unreadable discovery source: {other:?}"
                )),
            }
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_fifo() => {}
                Ok(_) => {
                    failures.push("raw read replaced the FIFO with another file type".to_string());
                }
                Err(error) => {
                    failures.push(format!("raw read removed the FIFO: {error}"));
                }
            }
            assert!(
                failures.is_empty(),
                "FIFO child failures:\n{}",
                failures.join("\n")
            );
            return;
        }

        let tmp = TempDir::new().unwrap();
        let fifo_path = tmp.path().join("daemon.fifo");
        let child_marker = tmp.path().join("child-started");
        let c_path = CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a live, NUL-terminated path and mode contains only
        // ordinary permission bits. The return code is checked before use.
        let result = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(result, 0, "mkfifo failed: {}", io::Error::last_os_error());

        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(TEST_NAME)
            .arg("--nocapture")
            .env(CHILD_PATH_ENV, &fifo_path)
            .env(CHILD_MARKER_ENV, &child_marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let child_status = loop {
            match child.try_wait().unwrap() {
                Some(status) => break Some(status),
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    break None;
                }
            }
        };

        let mut failures = Vec::new();
        match child_status {
            Some(status) if status.success() => {}
            Some(status) => failures.push(format!(
                "bounded FIFO child rejected the contract with status {status}"
            )),
            None => failures.push(
                "raw discovery read blocked on a FIFO past the two-second child watchdog"
                    .to_string(),
            ),
        }
        match std::fs::read(&child_marker) {
            Ok(marker) if marker == b"started" => {}
            Ok(marker) => failures.push(format!(
                "FIFO child wrote an unexpected start marker: {marker:?}"
            )),
            Err(error) => failures.push(format!(
                "FIFO child never reached the raw reader; exact filter may be wrong: {error}"
            )),
        }
        match std::fs::symlink_metadata(&fifo_path) {
            Ok(metadata) if metadata.file_type().is_fifo() => {}
            Ok(_) => {
                failures.push("watchdog run replaced the FIFO with another file type".to_string());
            }
            Err(error) => failures.push(format!("watchdog run removed the FIFO: {error}")),
        }

        assert!(
            failures.is_empty(),
            "FIFO raw discovery failures:\n{}",
            failures.join("\n")
        );
    }

    /// The state directory and the discovery file in it both name the `hyperd`
    /// endpoint, so both must be restricted to the owning user — including when
    /// an earlier release already created them under the process umask.
    ///
    /// Asserts the mode actually on disk rather than that a `chmod` was
    /// attempted, since only the former is what another local account sees.
    #[cfg(unix)]
    fn run_state_permissions_scenario() {
        use std::os::unix::fs::PermissionsExt as _;

        assert!(
            std::env::var_os("HYPERDB_STATE_DIR").is_some(),
            "child scenario requires an isolated state directory"
        );

        fn mode_of(path: &Path) -> u32 {
            std::fs::metadata(path)
                .unwrap_or_else(|error| panic!("{} should exist: {error}", path.display()))
                .permissions()
                .mode()
                & 0o777
        }

        let dir = state_dir().unwrap();
        let path = discovery_file_path().unwrap();
        let mut failures = Vec::new();

        // A state directory and discovery file created from nothing.
        write_discovery_file(&legacy_info()).unwrap();
        let fresh_dir_mode = mode_of(&dir);
        if fresh_dir_mode != 0o700 {
            failures.push(format!(
                "a newly created state directory was left at {fresh_dir_mode:04o} instead of \
                 0700, so another local account can traverse it"
            ));
        }
        let fresh_file_mode = mode_of(&path);
        if fresh_file_mode != 0o600 {
            failures.push(format!(
                "a newly written discovery file was left at {fresh_file_mode:04o} instead of \
                 0600, so it is readable by other local accounts"
            ));
        }

        // Permissions left loose by an earlier release must be corrected on the
        // next write rather than accepted as they are.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            (mode_of(&dir), mode_of(&path)),
            (0o755, 0o644),
            "fixture must start world-readable or it does not exercise the correction"
        );

        write_discovery_file(&legacy_info()).unwrap();
        let corrected_dir_mode = mode_of(&dir);
        if corrected_dir_mode != 0o700 {
            failures.push(format!(
                "a pre-existing world-readable state directory stayed at {corrected_dir_mode:04o} \
                 instead of being tightened to 0700"
            ));
        }
        let corrected_file_mode = mode_of(&path);
        if corrected_file_mode != 0o600 {
            failures.push(format!(
                "a pre-existing world-readable discovery file stayed at {corrected_file_mode:04o} \
                 instead of being tightened to 0600"
            ));
        }

        // The record must still be readable and intact afterwards: tightening
        // permissions is worthless if it breaks the daemon's own discovery.
        match serde_json::from_slice::<DaemonInfo>(&std::fs::read(&path).unwrap()) {
            Ok(parsed) if parsed == legacy_info() => {}
            Ok(_) => failures.push("the restricted record did not round-trip".to_string()),
            Err(error) => {
                failures.push(format!("the restricted record was unreadable: {error}"));
            }
        }

        assert!(
            failures.is_empty(),
            "daemon state permission failures:\n{}",
            failures.join("\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_directory_and_discovery_file_are_owner_only() {
        const CHILD_SENTINEL_ENV: &str = "HYPERDB_MCP_STATE_PERMISSIONS_CHILD";
        const TEST_NAME: &str =
            "daemon::discovery::tests::state_directory_and_discovery_file_are_owner_only";

        if let Some(marker) = std::env::var_os(CHILD_SENTINEL_ENV) {
            std::fs::write(std::path::PathBuf::from(marker), b"started").unwrap();
            run_state_permissions_scenario();
            return;
        }
        run_discovery_compatibility_child(TEST_NAME, CHILD_SENTINEL_ENV);
    }

    // ─── Legacy record hook ──────────────────────────────────────────────

    fn write_legacy_file(dir: &Path, pid: u32) -> PathBuf {
        let path = dir.join("daemon.json");
        let record = json!({
            "pid": pid,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_port": 7485,
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0",
        });
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        path
    }

    #[test]
    fn remove_legacy_record_removes_exactly_the_judged_record() {
        let tmp = TempDir::new().unwrap();
        let path = write_legacy_file(tmp.path(), 4_242);
        let judged = read_legacy_record(tmp.path()).expect("legacy record");
        assert_eq!(judged.pid, 4_242);

        remove_legacy_record(tmp.path(), judged);
        assert!(!path.exists());
    }

    #[test]
    fn remove_legacy_record_keeps_a_record_that_changed_since_it_was_read() {
        let tmp = TempDir::new().unwrap();
        write_legacy_file(tmp.path(), 4_242);
        let judged = read_legacy_record(tmp.path()).expect("legacy record");
        let path = write_legacy_file(tmp.path(), 9_999);

        remove_legacy_record(tmp.path(), judged);
        assert!(path.exists(), "a changed record must survive");
    }

    #[test]
    fn legacy_daemon_hint_names_pid_and_port_only_for_a_legacy_record() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(legacy_daemon_hint(tmp.path()), None);
        write_legacy_file(tmp.path(), 4_242);
        let hint = legacy_daemon_hint(tmp.path()).expect("legacy record gives a hint");
        assert!(hint.contains("pre-1.0 daemon"), "{hint}");
        assert!(hint.contains("4242"), "{hint}");
    }

    #[test]
    fn read_legacy_record_ignores_a_current_shape_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.json");
        std::fs::write(
            &path,
            json!({"pid": 1, "hyperd_endpoint": "x", "health_endpoint": "y",
                   "started_at": "t", "version": "1.0.0"})
            .to_string(),
        )
        .unwrap();
        assert_eq!(read_legacy_record(tmp.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_hooks_do_not_follow_a_symlinked_daemon_json() {
        let tmp = TempDir::new().unwrap();
        let target_dir = TempDir::new().unwrap();
        let target = write_legacy_file(target_dir.path(), 4_242);
        std::os::unix::fs::symlink(&target, tmp.path().join("daemon.json")).unwrap();

        assert_eq!(read_legacy_record(tmp.path()), None);
        let judged = read_legacy_record(target_dir.path()).unwrap();
        remove_legacy_record(tmp.path(), judged);
        assert!(target.exists(), "the symlink target must be left alone");
    }
}
