// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! TCP health listener for the daemon.
//!
//! The health listener serves two purposes:
//! 1. **Single-instance lock** — binding the port guarantees at most one daemon per user.
//! 2. **Liveness probe + heartbeat** — clients connect and send simple text commands.
//!
//! Protocol (line-based, newline-terminated):
//! - `PING\n` → `PONG\n` (liveness check)
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

use super::discovery::DaemonInfo;

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
                    "PING" => "PONG\n".to_string(),
                    "HEARTBEAT" => {
                        state.touch();
                        "OK\n".to_string()
                    }
                    "STOP" => {
                        state.request_shutdown();
                        "STOPPING\n".to_string()
                    }
                    "STATUS" => {
                        // Brief lock — only to clone the current snapshot.
                        let snapshot = info.lock().expect("DaemonInfo mutex poisoned").clone();
                        let json = serde_json::to_string(&snapshot).unwrap_or_default();
                        format!("{json}\n")
                    }
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
