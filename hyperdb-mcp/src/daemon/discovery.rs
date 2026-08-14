// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Discovery file management for the single-instance daemon.
//!
//! The daemon writes a JSON file to `~/.hyperdb/daemon.json` containing its
//! PID and the `hyperd` endpoint. Clients read this file to locate the running
//! daemon, validating liveness via a TCP health check before trusting it.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{DAEMON_PORT_SCAN_SPAN, DEFAULT_DAEMON_BASE_PORT};

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

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "crate-private interface for the side-effect-free doctor collector"
        )
    )]
    pub(crate) fn identity(&self) -> Option<&DaemonBuildIdentity> {
        self.identity.as_ref()
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "crate-private interface for the side-effect-free doctor collector"
    )
)]
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

    match serde_json::from_slice(&contents) {
        Ok(record) => RawDiscoveryRead::Parsed {
            path: reported_path,
            record,
        },
        Err(_) => RawDiscoveryRead::Malformed {
            path: reported_path,
        },
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
    let record = match read_discovery_file_raw(&path) {
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

    use serde_json::json;
    use tempfile::TempDir;

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
}
