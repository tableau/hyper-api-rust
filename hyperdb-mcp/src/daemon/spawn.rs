// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Spawn the daemon as a detached background process.
//!
//! When an MCP client starts and no daemon is running, it spawns one using the
//! current binary with the `daemon` subcommand. The spawned process is fully
//! detached so it outlives the parent MCP session.

use std::io;
use std::process::Command;
use std::time::{Duration, Instant};

use tracing::{debug, info};

use super::discovery::{self, DaemonInfo};

/// Maximum time to wait for the daemon to write its discovery file after spawning.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Polling interval while waiting for the discovery file.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Ensure a daemon is running and return its info.
/// If no daemon is detected, spawn one and wait for it to become ready.
///
/// # Errors
/// Returns an error if the daemon cannot be spawned or does not become ready
/// within the timeout period.
pub fn ensure_daemon(port: u16) -> io::Result<DaemonInfo> {
    // Check if already running
    if let Some(info) = discovery::discover() {
        debug!(endpoint = %info.hyperd_endpoint, "daemon already running");
        return Ok(info);
    }

    info!("no running daemon detected, spawning one");
    spawn_detached(port)?;
    wait_for_daemon()
}

/// Spawn `hyperdb-mcp daemon` as a fully detached background process.
fn spawn_detached(port: u16) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let port_str = port.to_string();

    let mut cmd = Command::new(&exe);
    cmd.arg("daemon").arg("--port").arg(&port_str);

    // Detach from parent: redirect stdio to null
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    // Platform-specific detach flags
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() is async-signal-safe per POSIX. Called in pre_exec
        // (between fork and exec) to create a new session so the daemon isn't
        // killed when the parent terminal/process exits.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }

    let child = cmd.spawn()?;
    info!(pid = child.id(), "daemon process spawned");
    Ok(())
}

/// Poll for the discovery file to appear (daemon is ready).
fn wait_for_daemon() -> io::Result<DaemonInfo> {
    let start = Instant::now();
    loop {
        if let Some(info) = discovery::discover() {
            info!(endpoint = %info.hyperd_endpoint, "daemon is ready");
            return Ok(info);
        }

        if start.elapsed() >= SPAWN_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "daemon did not become ready within {} seconds",
                    SPAWN_TIMEOUT.as_secs()
                ),
            ));
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}
