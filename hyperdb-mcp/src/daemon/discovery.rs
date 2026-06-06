// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Discovery file management for the single-instance daemon.
//!
//! The daemon writes a JSON file to `~/.hyperdb/daemon.json` containing its
//! PID and the `hyperd` endpoint. Clients read this file to locate the running
//! daemon, validating liveness via a TCP health check before trusting it.

use std::io;
use std::path::PathBuf;
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
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)?;

    let path = dir.join("daemon.json");
    let tmp_path = dir.join("daemon.json.tmp");
    let json = serde_json::to_string_pretty(info).map_err(|e| io::Error::other(e.to_string()))?;
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
    let contents = std::fs::read_to_string(&path).ok()?;
    let info: DaemonInfo = serde_json::from_str(&contents).ok()?;

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
