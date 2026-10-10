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

use std::path::Path;

use super::control::HealthEndpoint;
use super::discovery::{self, DaemonInfo};
use super::lock::DaemonLock;

/// Maximum time to wait for the daemon to write its discovery file after spawning.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Polling interval while waiting for the discovery file.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Ensure a daemon is running and return its info. If a daemon is found, we MAY
/// take it over if the client is newer. Otherwise we spawn a fresh one.
///
/// Whether a daemon exists is decided by the daemon lock, not by the record or
/// by probing: a held lock means a daemon is running, starting or stopping, so
/// the client waits for it; a free lock means none is, so a leftover record is
/// stale, is removed, and a daemon is spawned.
///
/// # Errors
/// Returns an error if the state directory is not private to the current user,
/// the daemon cannot be spawned, or it does not become ready within the timeout
/// period.
pub fn ensure_daemon() -> io::Result<DaemonInfo> {
    let dir = discovery::state_dir()?;
    ensure_daemon_in(&dir, spawn_detached, SPAWN_TIMEOUT)
}

/// [`ensure_daemon`] for an explicit state directory, spawner and wait budget.
fn ensure_daemon_in(
    dir: &Path,
    spawn: impl FnOnce() -> io::Result<()>,
    timeout: Duration,
) -> io::Result<DaemonInfo> {
    match super::state_perms::verify_state_dir_trusted(dir) {
        Ok(()) => {}
        // No directory yet: nobody has ever run a daemon here.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let start = Instant::now();
    loop {
        if let Some(info) = discovery::discover_in(dir) {
            return maybe_take_over(info, dir);
        }

        // The state directory may not exist yet; the daemon creates it.
        let lock = match std::fs::metadata(dir) {
            Ok(_) => DaemonLock::try_acquire(dir)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None::<DaemonLock>,
            Err(error) => return Err(error),
        };
        if let Some(lock) = lock {
            // Nothing holds the lock, so no daemon is running and any record
            // left behind is stale. Clean it up while we hold the lock so a
            // daemon cannot start between the check and the removal.
            discovery::remove_stale_record(dir);
            drop(lock);
            info!("no running daemon detected, spawning one");
            spawn()?;
            return wait_for_daemon(dir, timeout);
        }
        if !dir.exists() {
            info!("no state directory yet, spawning a daemon");
            spawn()?;
            return wait_for_daemon(dir, timeout);
        }

        // The lock is held but no live record answers: a daemon is starting
        // or stopping. Wait for it; spawning a second one would only lose the
        // race for the lock.
        if start.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "a daemon holds the lock in {} but did not become ready within {} seconds",
                    dir.display(),
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Spawn `hyperdb-mcp daemon` as a fully detached background process.
fn spawn_detached() -> io::Result<()> {
    let exe = std::env::current_exe()?;

    let mut cmd = Command::new(&exe);
    cmd.arg("daemon");

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
fn wait_for_daemon(dir: &Path, timeout: Duration) -> io::Result<DaemonInfo> {
    let start = Instant::now();
    loop {
        if let Some(info) = discovery::discover_in(dir) {
            info!(endpoint = %info.hyperd_endpoint, "daemon is ready");
            return Ok(info);
        }

        if start.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "daemon did not become ready within {} seconds",
                    timeout.as_secs()
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
/// If the client is newer, we send STOP to the old daemon, wait for it to stop
/// answering, then spawn a fresh daemon. Otherwise we reuse the existing daemon.
///
/// The health endpoint is the same for the old and the new daemon (it is derived
/// from the state directory), so a fresh daemon is told apart by its pid.
fn maybe_take_over(info: DaemonInfo, dir: &Path) -> io::Result<DaemonInfo> {
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
            endpoint = %info.health_endpoint,
            reason = if parse_failed { "version unparseable" } else { "client not newer" },
            "reusing existing daemon"
        );
        return Ok(info);
    }

    // Client is newer — take over.
    info!(
        daemon_version = %info.version,
        client_version = %client_ver,
        endpoint = %info.health_endpoint,
        "newer MCP client taking over older daemon"
    );

    // `discover_in` only returns a record whose endpoint it validated against
    // this state directory, so this parse succeeds for any `info` it produced.
    let Some(endpoint) = HealthEndpoint::from_record(&info.health_endpoint, dir) else {
        return Ok(info);
    };

    // Send STOP (best-effort; ignore error if daemon is already dying).
    let _ = super::health::send_command(&endpoint, "STOP");

    // Wait for the old daemon to stop answering.
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    while Instant::now() < deadline {
        if super::health::ping_identified(
            &endpoint,
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .is_none()
        {
            // The old daemon is gone, so spawn the new one.
            //
            // Another client could observe the same and spawn concurrently.
            // That is safe: the daemon lock lets exactly one daemon through, the
            // loser exits (or waits for the winner), and `wait_for_daemon`
            // converges on whichever daemon won.
            //
            // Narrowing that window: if a concurrent takeover has already
            // published a fresh daemon, adopt it instead of spawning.
            if let Some(fresh) = discovery::discover_in(dir)
                && fresh.pid != info.pid
            {
                return Ok(fresh);
            }
            spawn_detached()?;
            return wait_for_daemon(dir, SPAWN_TIMEOUT);
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Old daemon didn't die within the deadline — log a warning and reuse it
    // rather than fail the client.
    warn!(
        endpoint = %info.health_endpoint,
        timeout_secs = SPAWN_TIMEOUT.as_secs(),
        "old daemon did not stop within timeout, reusing it"
    );
    Ok(info)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// A spawner that only counts its calls and succeeds without starting
    /// anything.
    fn counting_spawner(calls: &Cell<u32>) -> impl FnOnce() -> io::Result<()> + '_ {
        move || {
            calls.set(calls.get() + 1);
            Ok(())
        }
    }

    /// A current-shape record whose endpoint nobody serves.
    fn write_stale_record(dir: &Path) -> std::path::PathBuf {
        let endpoint = HealthEndpoint::for_new_daemon(dir).expect("derive endpoint");
        let info = DaemonInfo {
            pid: 4_242,
            hyperd_endpoint: "127.0.0.1:1".to_string(),
            health_endpoint: endpoint.as_str().to_string(),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let path = dir.join("daemon.json");
        std::fs::write(&path, serde_json::to_vec(&info).expect("serialize record"))
            .expect("write stale record");
        path
    }

    /// A held lock with no answering daemon means one is starting or
    /// stopping: never spawn a second, time out instead.
    #[test]
    fn held_lock_without_a_record_never_spawns_and_times_out() {
        let dir = tempfile::tempdir().expect("state dir");
        let _held = DaemonLock::try_acquire(dir.path())
            .expect("lock probe")
            .expect("fresh dir is lockable");
        let calls = Cell::new(0);

        let started = Instant::now();
        let error = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(300),
        )
        .expect_err("a held lock with no daemon must not succeed");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 0, "the spawner must never run");
        assert!(started.elapsed() >= Duration::from_millis(300));
    }

    /// With the lock free, a leftover record is stale: it is removed under
    /// the lock and exactly one daemon is spawned.
    #[test]
    fn free_lock_with_a_stale_record_removes_it_and_spawns_once() {
        let dir = tempfile::tempdir().expect("state dir");
        let record = write_stale_record(dir.path());
        let calls = Cell::new(0);

        // The fake spawner starts nothing, so the wait afterwards times out;
        // what matters is what happened before it.
        let error = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(200),
        )
        .expect_err("the fake spawner publishes no daemon");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 1, "exactly one spawn");
        assert!(!record.exists(), "the stale record must have been removed");
    }

    /// No state directory yet: nobody ever ran a daemon here, so spawn one.
    #[test]
    fn missing_state_dir_spawns() {
        let parent = tempfile::tempdir().expect("parent dir");
        let dir = parent.path().join("never-created");
        let calls = Cell::new(0);

        let error = ensure_daemon_in(&dir, counting_spawner(&calls), Duration::from_millis(200))
            .expect_err("the fake spawner publishes no daemon");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 1, "the spawner must run once");
    }
}
