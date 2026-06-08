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

use tracing::{debug, info, warn};

use super::discovery::{self, DaemonInfo, PortScan, ScanOutcome};

/// Maximum time to wait for the daemon to write its discovery file after spawning.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Polling interval while waiting for the discovery file.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Ensure a daemon is running and return its info. If a daemon is found (via
/// discovery file or port scan), we MAY take it over if the client is newer.
/// Otherwise we spawn a fresh daemon on the first free port in the scan range.
///
/// # Errors
/// Returns an error if no free port is available, the daemon cannot be spawned,
/// or it does not become ready within the timeout period.
pub fn ensure_daemon(scan: PortScan) -> io::Result<DaemonInfo> {
    // Check discovery file first (fast path).
    if let Some(info) = discovery::discover() {
        return maybe_take_over(info, scan);
    }

    // Scan the port range for a running daemon or a free port.
    match discovery::scan_for_daemon(scan) {
        ScanOutcome::Found(info) => maybe_take_over(*info, scan),
        ScanOutcome::FreePort(port) => {
            info!(port, "no running daemon detected, spawning on free port");
            spawn_detached(port)?;
            let info = wait_for_daemon()?;
            // If the daemon we just spawned bound a port above the scan base (because
            // concurrent clients raced and one of them grabbed the base port first),
            // prefer the lower-port daemon so we don't accumulate redundant
            // daemon+hyperd pairs on adjacent ports. The lower-port daemon wins
            // because it bound first and is the canonical single instance.
            if info.health_port > scan.base {
                let lower_scan = PortScan {
                    base: scan.base,
                    span: info.health_port.saturating_sub(scan.base),
                };
                if let ScanOutcome::Found(lower_info) = discovery::scan_for_daemon(lower_scan) {
                    debug!(
                        prefer_port = lower_info.health_port,
                        stop_port = info.health_port,
                        "found lower-port daemon from concurrent spawn; stopping off-base daemon"
                    );
                    // Best-effort STOP — if it fails the off-base daemon idles
                    // harmlessly (it has no clients and will only cost background CPU).
                    let _ = super::health::send_command(info.health_port, "STOP");
                    return Ok(*lower_info);
                }
            }
            Ok(info)
        }
        ScanOutcome::AllOccupied => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "no free hyperdb daemon port in {}..{}",
                scan.base,
                scan.base.saturating_add(scan.span)
            ),
        )),
    }
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

/// Pure version comparison: returns `true` if the client should take over the daemon.
/// Only returns `true` when both versions parse successfully AND client > daemon.
/// Unparseable versions or equal/older client always return `false` (reuse daemon).
pub fn client_should_take_over(client_ver: &str, daemon_ver: &str) -> bool {
    let Ok(client) = semver::Version::parse(client_ver) else {
        return false;
    };
    let Ok(daemon) = semver::Version::parse(daemon_ver) else {
        return false;
    };
    client > daemon
}

/// Decide whether to reuse the running daemon or take it over with a newer version.
/// If the client is newer, we send STOP to the old daemon, wait for it to release
/// the port, then spawn a fresh daemon on the same port. Otherwise we reuse the
/// existing daemon.
///
/// The `scan` argument is intentionally unused for *where* to respawn: a takeover
/// always reuses the port the discovered daemon already holds (`info.health_port`),
/// because that is the port guaranteed to free up when the old daemon stops. A
/// mid-session change to `HYPERDB_DAEMON_PORT` (so the pinned `scan.base` differs
/// from `info.health_port`) is not honored here — spawning on a *different* port
/// would leave the old daemon alive and create two daemons rather than replace one.
/// That edge case is pathological (operators don't repin a live daemon) and the
/// daemon found via discovery is authoritative for its own port.
fn maybe_take_over(info: DaemonInfo, _scan: PortScan) -> io::Result<DaemonInfo> {
    let client_ver = crate::version::MCP_VERSION;

    if !client_should_take_over(client_ver, &info.version) {
        // Client is older or equal, or one/both versions failed to parse → reuse.
        // Distinguish the two reasons so an unexpected unparseable daemon version
        // (corrupt daemon.json, foreign writer) is visible when debugging.
        let parse_failed = semver::Version::parse(client_ver).is_err()
            || semver::Version::parse(&info.version).is_err();
        debug!(
            daemon_version = %info.version,
            client_version = %client_ver,
            port = info.health_port,
            reason = if parse_failed { "version unparseable" } else { "client not newer" },
            "reusing existing daemon"
        );
        return Ok(info);
    }

    // Client is newer — take over.
    info!(
        daemon_version = %info.version,
        client_version = %client_ver,
        port = info.health_port,
        "newer MCP client taking over older daemon"
    );

    // Send STOP (best-effort; ignore error if daemon is already dying).
    let _ = super::health::send_command(info.health_port, "STOP");

    // Wait for the old daemon to release the port (confirmed by ping_identified returning None).
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    while Instant::now() < deadline {
        if super::health::ping_identified(
            info.health_port,
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .is_none()
        {
            // Port is free — spawn the new daemon on the same port.
            //
            // There is a benign TOCTOU window here: another client could also
            // observe the freed port and spawn concurrently. That is safe by the
            // same argument as the FreePort path — `spawn_detached` is
            // fire-and-forget (it does not itself bind the port; the spawned
            // daemon's `HealthListener::bind` is the real single-instance lock).
            // The OS grants the bind to exactly one daemon; the loser exits at
            // step 1 of `run_daemon` (before spawning hyperd or writing the
            // discovery file), and `wait_for_daemon` (which polls `discover()`)
            // converges on whichever daemon won. No duplicate daemon survives
            // and no AddrInUse surfaces to the client.
            //
            // Defensive narrowing of that window: if a concurrent takeover has
            // already published a fresh, identity-verified daemon on this port,
            // adopt it instead of spawning — this avoids returning the stale
            // `info` (old endpoint) we were carrying and skips a redundant spawn.
            if let Some(fresh) = discovery::discover() {
                if fresh.health_port == info.health_port {
                    return Ok(fresh);
                }
            }
            spawn_detached(info.health_port)?;
            return wait_for_daemon();
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Old daemon didn't die within the deadline — log a warning and reuse it
    // rather than fail the client.
    warn!(
        port = info.health_port,
        timeout_secs = SPAWN_TIMEOUT.as_secs(),
        "old daemon did not stop within timeout, reusing it"
    );
    Ok(info)
}
