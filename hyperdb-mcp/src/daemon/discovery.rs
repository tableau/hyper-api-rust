// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Discovery file management for the single-instance daemon.
//!
//! The daemon writes a JSON file to `~/.hyperdb/daemon.json` containing its
//! PID and the `hyperd` endpoint. Clients read this file to locate the running
//! daemon, validating liveness via a TCP health check before trusting it.

use std::io;
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::DEFAULT_DAEMON_PORT;

/// Information written by the daemon so clients can discover and connect.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Check if the daemon is alive by attempting a TCP connection to its health port.
fn is_daemon_alive(port: u16) -> bool {
    TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    )
    .is_ok()
}

/// Resolve the daemon health port from environment or default.
pub fn resolve_port() -> u16 {
    std::env::var(super::ENV_DAEMON_PORT)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DAEMON_PORT)
}

/// Cross-platform home directory resolution.
fn home_dir() -> Option<PathBuf> {
    // Try HOME (Unix) then USERPROFILE (Windows)
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
