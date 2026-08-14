// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! TCP health listener for the daemon.
//!
//! The health listener serves two purposes:
//! 1. **Single-instance lock** — binding the port guarantees at most one daemon per user.
//! 2. **Liveness probe + heartbeat** — clients connect and send simple text commands.
//!
//! Protocol (line-based, newline-terminated):
//! - `PING\n` → `PONG hyperdb-mcp <version>\n` (liveness check; the identifying
//!   token proves it's a hyperdb-mcp daemon, not a foreign process on the same port)
//! - `HEARTBEAT\n` → `OK\n` (resets idle timer)
//! - `STOP\n` → `STOPPING\n` (triggers graceful shutdown)
//! - `STATUS\n` → JSON line with daemon info (reports the *current* hyperd
//!   endpoint, which can change after a restart).
//! - `REPORT_HYPERD_ERROR\n` → `OK\n` (sets the restart-requested flag —
//!   the monitor task picks it up on its next tick).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use super::discovery::{DaemonInfo, DaemonRecord};

/// Identifying token included in PONG responses. Used to verify that a bound
/// port is owned by a hyperdb-mcp daemon (not a foreign service).
pub const PONG_TOKEN: &str = "hyperdb-mcp";

/// Construct the PONG response with the identifying token and version.
fn pong_response() -> String {
    format!("PONG {PONG_TOKEN} {}\n", crate::version::MCP_VERSION)
}

/// Handle to the health listener, used to check binding success and manage lifecycle.
#[derive(Debug)]
pub struct HealthListener {
    listener: TcpListener,
    pub port: u16,
}

/// Shared state between the health listener and the daemon main loop.
#[derive(Debug)]
pub struct DaemonState {
    /// Last time any client sent a heartbeat or query.
    pub last_activity: Mutex<Instant>,
    /// Signal to shut down the daemon.
    pub shutdown: AtomicBool,
    /// Set by clients reporting that hyperd looks dead from over there;
    /// consumed by the daemon's restart monitor.
    pub restart_requested: AtomicBool,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            last_activity: Mutex::new(Instant::now()),
            shutdown: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
        }
    }

    /// Record activity (resets idle timer).
    ///
    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn touch(&self) {
        *self.last_activity.lock().expect("mutex poisoned") = Instant::now();
    }

    /// Duration since the last activity.
    ///
    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn idle_duration(&self) -> Duration {
        self.last_activity.lock().expect("mutex poisoned").elapsed()
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    pub fn should_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Signal that hyperd appears to have died and a restart is needed.
    pub fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::Release);
    }

    /// Atomically read-and-clear the restart-request flag.
    /// Returns true if a restart was requested since the last call.
    pub fn consume_restart_request(&self) -> bool {
        self.restart_requested.swap(false, Ordering::AcqRel)
    }
}

impl HealthListener {
    /// Try to bind the health port.
    ///
    /// # Errors
    /// Returns `Err` if the port is already in use (another daemon is running)
    /// or the bind fails for another reason.
    pub fn bind(port: u16) -> std::io::Result<Self> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    /// Run the health listener loop. Spawns per-connection threads until shutdown.
    /// Consumes `self` because this is intended to be called from a dedicated thread.
    ///
    /// `info` is shared (`Arc<Mutex<DaemonInfo>>`) so the listener reports the
    /// *current* hyperd endpoint after a restart — the monitor task updates the
    /// same Arc once a new hyperd is running.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "Arcs are cloned into per-connection threads"
    )]
    pub fn run(self, state: Arc<DaemonState>, info: Arc<Mutex<DaemonInfo>>) {
        loop {
            if state.should_shutdown() {
                break;
            }

            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    let state = Arc::clone(&state);
                    let info = Arc::clone(&info);
                    std::thread::spawn(move || {
                        handle_client(stream, &state, &info);
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    warn!(error = %e, "health listener accept error");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
        debug!("health listener shut down");
    }
}

fn status_json(info: &Mutex<DaemonInfo>) -> String {
    let snapshot = info.lock().expect("DaemonInfo mutex poisoned").clone();
    match DaemonRecord::with_current_identity(&snapshot) {
        Ok(record) => serde_json::to_string(&record).unwrap_or_default(),
        Err(error) => {
            warn!(%error, "could not collect daemon executable identity for STATUS");
            serde_json::to_string(&snapshot).unwrap_or_default()
        }
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "TcpStream must be owned for BufReader"
)]
fn handle_client(stream: TcpStream, state: &DaemonState, info: &Mutex<DaemonInfo>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(&stream);
    let mut writer = &stream;
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let cmd = line.trim();
                let response = match cmd {
                    "PING" => pong_response(),
                    "HEARTBEAT" => {
                        state.touch();
                        "OK\n".to_string()
                    }
                    "STOP" => {
                        state.request_shutdown();
                        "STOPPING\n".to_string()
                    }
                    "STATUS" => format!("{}\n", status_json(info)),
                    "REPORT_HYPERD_ERROR" => {
                        state.request_restart();
                        "OK\n".to_string()
                    }
                    _ => "ERR unknown command\n".to_string(),
                };
                if writer.write_all(response.as_bytes()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Send a command to the daemon's health port and return the response.
///
/// Uses generous timeouts (2s connect, 5s read) suitable for `STOP`/`STATUS`
/// where the caller is willing to wait. Use [`send_command_with_timeout`] for
/// best-effort fire-and-forget calls (e.g. heartbeat, error reporting).
///
/// # Errors
/// Returns an error if the connection fails or the response cannot be read.
pub fn send_command(port: u16, command: &str) -> std::io::Result<String> {
    send_command_with_timeout(
        port,
        command,
        Duration::from_secs(2),
        Duration::from_secs(5),
    )
}

/// Best-effort fire-and-forget: tell the running daemon that hyperd appears to
/// be dead from this client's perspective. Uses short timeouts (200ms each) so
/// the calling tool handler isn't stalled if the daemon itself is slow.
/// Errors are logged at debug level and otherwise ignored.
pub fn report_hyperd_error_to_daemon() {
    let port = super::discovery::resolve_port();
    let timeout = Duration::from_millis(200);
    match send_command_with_timeout(port, "REPORT_HYPERD_ERROR", timeout, timeout) {
        Ok(response) => {
            debug!(response = %response.trim(), "reported hyperd error to daemon");
        }
        Err(e) => {
            debug!(error = %e, "could not report hyperd error to daemon (best-effort)");
        }
    }
}

/// Send a command with caller-specified connect/read timeouts.
///
/// # Errors
/// Returns an error if the connection fails or the response cannot be read
/// within the supplied timeouts.
pub fn send_command_with_timeout(
    port: u16,
    command: &str,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> std::io::Result<String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, connect_timeout)?;
    stream.set_read_timeout(Some(read_timeout))?;

    let msg = format!("{command}\n");
    stream.write_all(msg.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response)
}

/// Send PING and verify the response contains the identifying token. Returns
/// `Some(version)` if the responding daemon is a hyperdb-mcp daemon (the version
/// string is the daemon's `MCP_VERSION`), or `None` if connection fails, the
/// response lacks the expected token, or read times out. An empty version string
/// (`Some(String::new())`) is returned if the PONG prefix matches but no version
/// token is present (graceful degradation for forward/backward compat).
///
/// This is the primitive for liveness checks now that bare TCP connect is
/// insufficient (a foreign service on the same port would cause collisions).
pub fn ping_identified(
    port: u16,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Option<String> {
    let response = send_command_with_timeout(port, "PING", connect_timeout, read_timeout).ok()?;
    // Validate by exact tokens, not a string prefix: a prefix check on
    // "PONG hyperdb-mcp" would also match a foreign reply like
    // "PONG hyperdb-mcpEVIL 1.0.0". Require the first two whitespace-separated
    // tokens to be exactly "PONG" and the token, so only our daemon passes.
    let mut tokens = response.split_whitespace();
    if tokens.next() != Some("PONG") || tokens.next() != Some(PONG_TOKEN) {
        return None;
    }
    // The 3rd token is the daemon's version; absent ⇒ accept with empty
    // version (future-proofing for a token-only reply).
    Some(tokens.next().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use serde_json::{json, Value};

    use crate::diagnostics::ReportedPath;

    use super::*;

    fn daemon_info(health_port: u16) -> DaemonInfo {
        DaemonInfo {
            pid: 4242,
            hyperd_endpoint: "127.0.0.1:54321".to_string(),
            health_port,
            started_at: "2026-08-13T12:34:56Z".to_string(),
            version: "0.7.0".to_string(),
        }
    }

    fn expected_status(info: &DaemonInfo) -> Value {
        let executable = std::env::current_exe().unwrap();
        let executable_path = ReportedPath::from_os_str(executable.as_os_str());

        json!({
            "pid": info.pid,
            "hyperd_endpoint": info.hyperd_endpoint,
            "health_port": info.health_port,
            "started_at": info.started_at,
            "version": info.version,
            "identity": {
                "mcp_version": crate::version::mcp_version_string(),
                "executable_path": executable_path
            }
        })
    }

    fn check_status_json(
        label: &str,
        response: &str,
        expected: &Value,
        failures: &mut Vec<String>,
    ) {
        match serde_json::from_str::<Value>(response.trim()) {
            Ok(actual) => {
                if actual != *expected {
                    failures.push(format!(
                        "{label} was not the exact flat enriched record: {actual}"
                    ));
                }
                if actual.get("info").is_some() {
                    failures.push(format!("{label} nested legacy fields under `info`"));
                }
            }
            Err(error) => failures.push(format!("{label} was not JSON: {error}")),
        }
    }

    #[test]
    fn health_status_returns_flat_enriched_record() {
        let public_run_signature: fn(HealthListener, Arc<DaemonState>, Arc<Mutex<DaemonInfo>>) =
            HealthListener::run;
        std::hint::black_box(public_run_signature);

        let listener = HealthListener::bind(0).unwrap();
        let port = listener.port;
        let state = Arc::new(DaemonState::new());
        let info = Arc::new(Mutex::new(daemon_info(port)));
        let initial_expected = expected_status(&info.lock().unwrap());
        let mut failures = Vec::new();

        match catch_unwind(AssertUnwindSafe(|| status_json(info.as_ref()))) {
            Ok(response) => check_status_json(
                "private STATUS serializer",
                &response,
                &initial_expected,
                &mut failures,
            ),
            Err(_) => failures.push("private STATUS serializer is not implemented".to_string()),
        }

        let run_state = Arc::clone(&state);
        let run_info = Arc::clone(&info);
        let handle = std::thread::spawn(move || listener.run(run_state, run_info));

        let initial_response = send_command(port, "STATUS").unwrap();
        check_status_json(
            "initial STATUS response",
            &initial_response,
            &initial_expected,
            &mut failures,
        );

        {
            let mut current = info.lock().unwrap();
            current.hyperd_endpoint = "127.0.0.1:60000".to_string();
        }
        let updated_expected = expected_status(&info.lock().unwrap());
        let updated_response = send_command(port, "STATUS").unwrap();
        check_status_json(
            "STATUS response after shared DaemonInfo update",
            &updated_response,
            &updated_expected,
            &mut failures,
        );

        let _ = send_command(port, "STOP");
        handle.join().unwrap();

        assert!(
            failures.is_empty(),
            "health STATUS contract failures:\n{}",
            failures.join("\n")
        );
    }
}
