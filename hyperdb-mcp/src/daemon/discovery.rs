// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Discovery file management for the single-instance daemon.
//!
//! The daemon writes a JSON file to `~/.hyperdb/daemon.json` containing its
//! PID and the `hyperd` endpoint. Clients read this file to locate the running
//! daemon, validating liveness via a TCP health check before trusting it.

use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{DAEMON_PORT_SCAN_SPAN, DEFAULT_DAEMON_BASE_PORT};

const MAX_DISCOVERY_FILE_BYTES: usize = 64 * 1024;

/// Information written by the daemon so clients can discover and connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// OS process ID of the daemon.
    pub pid: u32,
    /// The `hyperd` libpq endpoint clients should connect to (e.g. `127.0.0.1:54321`).
    pub hyperd_endpoint: String,
    /// The TCP port the daemon's health listener is bound to.
    pub health_port: u16,
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
        return RawDiscoveryRead::Malformed {
            path: reported_path,
        };
    }

    parse_discovery_contents(reported_path, &contents)
}

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
    parse_discovery_contents(reported_path, &contents)
}

fn parse_discovery_contents(
    reported_path: crate::diagnostics::ReportedPath,
    contents: &[u8],
) -> RawDiscoveryRead {
    match serde_json::from_slice(contents) {
        Ok(record) => RawDiscoveryRead::Parsed {
            path: reported_path,
            record,
        },
        Err(_) => RawDiscoveryRead::Malformed {
            path: reported_path,
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
    write_discovery_record(info)
}

pub(super) fn write_enriched_discovery_file(info: &DaemonInfo) -> io::Result<()> {
    let record = DaemonRecord::with_current_identity(info)?;
    write_discovery_record(&record)
}

fn write_discovery_record(record: &(impl Serialize + ?Sized)) -> io::Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)?;

    let path = dir.join("daemon.json");
    let tmp_path = dir.join("daemon.json.tmp");
    let json = serde_json::to_string_pretty(record).map_err(|e| io::Error::other(e.to_string()))?;
    std::fs::write(&tmp_path, json.as_bytes())?;
    // On Windows, rename fails if target exists. Remove stale target first.
    let _ = std::fs::remove_file(&path);
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Read the discovery file and validate that the daemon is still alive.
/// Returns `None` if no daemon is running (file missing, stale, or unreachable).
pub fn discover() -> Option<DaemonInfo> {
    let path = discovery_file_path().ok()?;
    // Preserve the historical client-discovery contract: normal discovery
    // follows symlinks and accepts any valid record size. Doctor uses the
    // separate bounded, no-follow raw reader above because it must never
    // mutate or block on a special file.
    let record = match read_discovery_file_legacy(&path) {
        RawDiscoveryRead::Missing { path } => {
            tracing::debug!(encoding = ?path.encoding, "daemon discovery file is missing");
            return None;
        }
        RawDiscoveryRead::Unreadable { path, kind } => {
            tracing::debug!(?kind, encoding = ?path.encoding, "daemon discovery file is unreadable");
            return None;
        }
        RawDiscoveryRead::Malformed { path } => {
            tracing::debug!(encoding = ?path.encoding, "daemon discovery file is malformed");
            return None;
        }
        RawDiscoveryRead::Parsed { path, record } => {
            tracing::debug!(encoding = ?path.encoding, "daemon discovery file parsed");
            record
        }
    };
    let info = record.info().clone();

    // Validate liveness by connecting to the health port
    if is_daemon_alive(info.health_port) {
        Some(info)
    } else {
        // Stale file — daemon crashed. Clean up.
        let _ = std::fs::remove_file(&path);
        None
    }
}

/// Remove the discovery file (called during graceful shutdown).
pub fn remove_discovery_file() {
    if let Ok(path) = discovery_file_path() {
        let _ = std::fs::remove_file(&path);
    }
}

/// Check if the daemon is alive by sending PING and verifying the identifying token.
/// No longer accepts a bare TCP connect (prevents collisions with foreign services).
fn is_daemon_alive(port: u16) -> bool {
    super::health::ping_identified(port, Duration::from_millis(300), Duration::from_millis(300))
        .is_some()
}

/// Port scan configuration: a base port and the number of ports to scan.
/// When `span == 1`, the port is pinned (no scan). Used by the later
/// port-scanning stage to discover or spawn a daemon across a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortScan {
    pub base: u16,
    pub span: u16,
}

/// Resolve the daemon health port scan configuration from environment or default.
/// If `HYPERDB_DAEMON_PORT` is set and valid, returns a pinned scan (span=1) at
/// that exact port. Otherwise, returns the default base port with the full scan span.
pub fn resolve_port_scan() -> PortScan {
    if let Some(port) = std::env::var(super::ENV_DAEMON_PORT)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
    {
        PortScan {
            base: port,
            span: 1,
        }
    } else {
        PortScan {
            base: DEFAULT_DAEMON_BASE_PORT,
            span: DAEMON_PORT_SCAN_SPAN,
        }
    }
}

/// Resolve the daemon health port from environment or default. Back-compat
/// wrapper for single-port callers; returns the base port from [`resolve_port_scan`].
/// New code that needs scan-aware logic should call [`resolve_port_scan`] directly.
pub fn resolve_port() -> u16 {
    resolve_port_scan().base
}

/// Cross-platform home directory resolution.
fn home_dir() -> Option<PathBuf> {
    // Try HOME (Unix) then USERPROFILE (Windows)
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Result of probing a single port: either our daemon, something else, or refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// A hyperdb-mcp daemon answered with valid STATUS.
    OurDaemon(Box<DaemonInfo>),
    /// The port accepted TCP but isn't our daemon (foreign service or broken STATUS).
    Camped,
    /// Connection refused (port is free).
    Refused,
}

/// Probe a single port to determine if it's occupied by our daemon, a foreign service, or free.
fn probe_port(port: u16) -> ProbeResult {
    let ping_timeout = Duration::from_millis(300);

    if let Some(_version) = super::health::ping_identified(port, ping_timeout, ping_timeout) {
        // PING succeeded — something is answering with our token. Now send STATUS
        // to retrieve the full daemon info. If STATUS fails we can't trust this
        // process (might be a test stub or a broken daemon), so treat it as Camped.
        match super::health::send_command_with_timeout(port, "STATUS", ping_timeout, ping_timeout) {
            Ok(response) => {
                if let Ok(info) = serde_json::from_str::<DaemonInfo>(response.trim()) {
                    ProbeResult::OurDaemon(Box::new(info))
                } else {
                    // Parsed PING but STATUS is malformed — treat as Camped.
                    ProbeResult::Camped
                }
            }
            Err(_) => ProbeResult::Camped,
        }
    } else {
        // PING failed or returned no identifying token. Distinguish "refused"
        // from "camped non-daemon" via a raw TCP connect attempt.
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        match std::net::TcpStream::connect_timeout(&addr, ping_timeout) {
            Ok(_) => ProbeResult::Camped, // TCP accepted but PING failed → foreign
            Err(_) => ProbeResult::Refused, // Connection refused → port is free
        }
    }
}

/// The outcome of scanning a port range for a running daemon or a free port to spawn on.
#[derive(Debug)]
pub enum ScanOutcome {
    /// Found a running hyperdb-mcp daemon.
    Found(Box<DaemonInfo>),
    /// No daemon found, but this port is free (can spawn here).
    FreePort(u16),
    /// All ports in the range are occupied (either by our daemon, foreign services, or both).
    AllOccupied,
}

/// Scan the configured port range to find a running daemon or identify a free port.
/// If any port in the range answers identified-PING and returns valid STATUS, we return
/// `Found` immediately (first wins). Otherwise, we return `FreePort` with the first
/// refused port encountered, or `AllOccupied` if everything is in use.
///
/// Product decision: prefer finding an existing daemon anywhere in range over
/// spawning a new one. Only spawn if no daemon exists.
pub fn scan_for_daemon(scan: PortScan) -> ScanOutcome {
    let mut first_free: Option<u16> = None;

    for offset in 0..scan.span {
        let Some(port) = scan.base.checked_add(offset) else {
            break; // Overflow guard: stop at u16::MAX
        };

        match probe_port(port) {
            ProbeResult::OurDaemon(info) => {
                // Found a running daemon — return immediately.
                return ScanOutcome::Found(info);
            }
            ProbeResult::Refused => {
                // Port is free. Remember the first one we see.
                if first_free.is_none() {
                    first_free = Some(port);
                }
            }
            ProbeResult::Camped => {
                // Port is occupied by something else. Keep scanning.
            }
        }
    }

    // No daemon found. Return the first free port, or AllOccupied if none.
    match first_free {
        Some(port) => ScanOutcome::FreePort(port),
        None => ScanOutcome::AllOccupied,
    }
}

/// Discover a running daemon via the discovery file, or by scanning the configured
/// port range. Returns `None` if no daemon is found in either place.
///
/// Used by CLI commands (status/stop) that want to find a daemon but not spawn one.
pub fn find_running_daemon() -> Option<DaemonInfo> {
    discover().or_else(|| match scan_for_daemon(resolve_port_scan()) {
        ScanOutcome::Found(info) => Some(*info),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tempfile::TempDir;

    use crate::daemon::health::{DaemonState, HealthListener};
    use crate::diagnostics::{PathEncoding, ReportedPath};

    use super::*;

    fn legacy_info() -> DaemonInfo {
        DaemonInfo {
            pid: 4242,
            hyperd_endpoint: "127.0.0.1:54321".to_string(),
            health_port: 7485,
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
            "health_port": 7485,
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
            "health_port": 7485,
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
            "health_port": 7485,
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
                if format!("{state:?}").contains(SECRET_SENTINEL) {
                    failures.push("malformed state leaked discovery contents".to_string());
                }
                match state {
                    RawDiscoveryRead::Malformed { path }
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
    fn raw_discovery_rejects_oversized_valid_json() {
        const MAX_EXPECTED_DISCOVERY_BYTES: usize = 64 * 1024;

        let tmp = TempDir::new().unwrap();
        let base_bytes = serde_json::to_vec(&json!({
            "pid": 4242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_port": 7485,
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
            RawDiscoveryRead::Malformed { path: reported }
                if reported == ReportedPath::from_os_str(oversized_path.as_os_str()) => {}
            other => failures.push(format!(
                "oversized valid JSON was not rejected as malformed: {other:?}"
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

    fn run_oversized_discovery_compatibility_scenario() {
        const RAW_DOCTOR_LIMIT_BYTES: usize = 64 * 1024;

        assert!(
            std::env::var_os("HYPERDB_STATE_DIR").is_some(),
            "child scenario requires an isolated state directory"
        );
        let health_listener = HealthListener::bind(0).unwrap();
        let health_port = health_listener.port;
        let oversized_info = DaemonInfo {
            pid: 5_252,
            hyperd_endpoint: "127.0.0.1:54321".to_string(),
            health_port,
            started_at: "2026-08-13T12:34:56Z".to_string(),
            version: "v".repeat(RAW_DOCTOR_LIMIT_BYTES + 1),
        };
        write_discovery_file(&oversized_info).unwrap();
        let path = discovery_file_path().unwrap();
        let original_bytes = std::fs::read(&path).unwrap();
        let mut failures = Vec::new();

        if original_bytes.len() <= RAW_DOCTOR_LIMIT_BYTES {
            failures.push(format!(
                "public writer produced only {} bytes, expected more than {RAW_DOCTOR_LIMIT_BYTES}",
                original_bytes.len()
            ));
        }
        match serde_json::from_slice::<DaemonInfo>(&original_bytes) {
            Ok(parsed) if parsed == oversized_info => {}
            Ok(_) => {
                failures.push("oversized public DaemonInfo did not round-trip exactly".to_string());
            }
            Err(error) => failures.push(format!(
                "public writer did not produce valid oversized DaemonInfo JSON: {error}"
            )),
        }
        match read_discovery_file_raw(&path) {
            RawDiscoveryRead::Malformed { path: reported }
                if reported == ReportedPath::from_os_str(path.as_os_str()) => {}
            RawDiscoveryRead::Missing { .. } => {
                failures
                    .push("doctor raw reader reported the oversized record missing".to_string());
            }
            RawDiscoveryRead::Unreadable { kind, .. } => failures.push(format!(
                "doctor raw reader reported the oversized record unreadable: {kind:?}"
            )),
            RawDiscoveryRead::Malformed { .. } => {
                failures.push("doctor raw reader reported the wrong oversized path".to_string());
            }
            RawDiscoveryRead::Parsed { .. } => {
                failures.push("doctor raw reader accepted the oversized record".to_string());
            }
        }
        match std::fs::read(&path) {
            Ok(after) if after == original_bytes => {}
            Ok(_) => failures.push("doctor raw reader changed oversized bytes".to_string()),
            Err(error) => failures.push(format!(
                "doctor raw reader removed the oversized record: {error}"
            )),
        }

        let health_state = Arc::new(DaemonState::new());
        let health_info = Arc::new(Mutex::new(oversized_info.clone()));
        let run_state = Arc::clone(&health_state);
        let run_info = Arc::clone(&health_info);
        let health_server = std::thread::spawn(move || health_listener.run(run_state, run_info));

        match discover() {
            Some(info) if info == oversized_info => {}
            Some(_) => {
                failures
                    .push("normal discover returned different live oversized facts".to_string());
            }
            None => failures
                .push("normal discover did not accept the live oversized record".to_string()),
        }
        match std::fs::read(&path) {
            Ok(after) if after == original_bytes => {}
            Ok(_) => failures.push("live discover changed oversized bytes".to_string()),
            Err(error) => failures.push(format!(
                "live discover removed the oversized record: {error}"
            )),
        }

        health_state.request_shutdown();
        health_server.join().unwrap();
        if discover().is_some() {
            failures.push("stopped oversized record was incorrectly retained as live".to_string());
        }
        if path.exists() {
            failures
                .push("normal discover did not stale-clean the valid oversized record".to_string());
        }

        assert!(
            failures.is_empty(),
            "oversized legacy discover failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn discover_preserves_legacy_oversized_stale_cleanup() {
        const CHILD_SENTINEL_ENV: &str = "HYPERDB_MCP_OVERSIZED_DISCOVERY_COMPATIBILITY_CHILD";
        const TEST_NAME: &str =
            "daemon::discovery::tests::discover_preserves_legacy_oversized_stale_cleanup";

        let _process_guard = crate::diagnostics::real_network_test_guard();
        if let Some(marker) = std::env::var_os(CHILD_SENTINEL_ENV) {
            std::fs::write(std::path::PathBuf::from(marker), b"started").unwrap();
            run_oversized_discovery_compatibility_scenario();
            return;
        }
        run_discovery_compatibility_child(TEST_NAME, CHILD_SENTINEL_ENV);
    }

    #[cfg(unix)]
    fn run_symlink_discovery_compatibility_scenario() {
        assert!(
            std::env::var_os("HYPERDB_STATE_DIR").is_some(),
            "child scenario requires an isolated state directory"
        );
        let health_listener = HealthListener::bind(0).unwrap();
        let health_port = health_listener.port;
        let mut linked_info = legacy_info();
        linked_info.pid = 6_363;
        linked_info.health_port = health_port;
        write_discovery_file(&linked_info).unwrap();
        let link_path = discovery_file_path().unwrap();
        let target_path = link_path.with_file_name("legacy-daemon-target.json");
        std::fs::rename(&link_path, &target_path).unwrap();
        std::os::unix::fs::symlink(&target_path, &link_path).unwrap();
        let target_bytes = std::fs::read(&target_path).unwrap();
        let mut failures = Vec::new();

        match read_discovery_file_raw(&link_path) {
            RawDiscoveryRead::Unreadable {
                path: reported,
                kind,
            } if reported == ReportedPath::from_os_str(link_path.as_os_str())
                && kind != io::ErrorKind::NotFound => {}
            other => failures.push(format!(
                "doctor raw reader did not reject the symlink without following it: {other:?}"
            )),
        }
        match std::fs::symlink_metadata(&link_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {}
            Ok(metadata) => failures.push(format!(
                "doctor raw reader replaced the link with {:?}",
                metadata.file_type()
            )),
            Err(error) => failures.push(format!(
                "doctor raw reader removed the discovery symlink: {error}"
            )),
        }
        match std::fs::read(&target_path) {
            Ok(after) if after == target_bytes => {}
            Ok(_) => failures.push("doctor raw reader changed symlink target bytes".to_string()),
            Err(error) => failures.push(format!(
                "doctor raw reader removed the symlink target: {error}"
            )),
        }

        let health_state = Arc::new(DaemonState::new());
        let health_info = Arc::new(Mutex::new(linked_info.clone()));
        let run_state = Arc::clone(&health_state);
        let run_info = Arc::clone(&health_info);
        let health_server = std::thread::spawn(move || health_listener.run(run_state, run_info));

        match discover() {
            Some(info) if info == linked_info => {}
            other => failures.push(format!(
                "normal discover did not follow the live valid symlink: {other:?}"
            )),
        }
        match std::fs::symlink_metadata(&link_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {}
            Ok(_) => failures.push("live discover replaced the discovery symlink".to_string()),
            Err(error) => failures.push(format!(
                "live discover removed the discovery symlink: {error}"
            )),
        }

        health_state.request_shutdown();
        health_server.join().unwrap();
        if discover().is_some() {
            failures.push("stopped symlinked record was incorrectly retained as live".to_string());
        }
        match std::fs::symlink_metadata(&link_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => failures
                .push("normal discover did not remove the stale discovery symlink".to_string()),
            Err(error) => failures.push(format!(
                "stale discovery symlink cleanup failed unexpectedly: {error}"
            )),
        }
        match std::fs::read(&target_path) {
            Ok(after) if after == target_bytes => {}
            Ok(_) => failures.push("normal discover changed symlink target bytes".to_string()),
            Err(error) => failures.push(format!(
                "normal discover removed the symlink target instead of the link: {error}"
            )),
        }

        assert!(
            failures.is_empty(),
            "symlink legacy discover failures:\n{}",
            failures.join("\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn discover_preserves_legacy_symlink_stale_cleanup() {
        const CHILD_SENTINEL_ENV: &str = "HYPERDB_MCP_SYMLINK_DISCOVERY_COMPATIBILITY_CHILD";
        const TEST_NAME: &str =
            "daemon::discovery::tests::discover_preserves_legacy_symlink_stale_cleanup";

        let _process_guard = crate::diagnostics::real_network_test_guard();
        if let Some(marker) = std::env::var_os(CHILD_SENTINEL_ENV) {
            std::fs::write(std::path::PathBuf::from(marker), b"started").unwrap();
            run_symlink_discovery_compatibility_scenario();
            return;
        }
        run_discovery_compatibility_child(TEST_NAME, CHILD_SENTINEL_ENV);
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
}
