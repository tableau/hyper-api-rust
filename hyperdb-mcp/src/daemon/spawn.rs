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
use super::legacy;
use super::lock::DaemonLock;

/// Maximum time to wait for the daemon to write its discovery file after
/// spawning. It covers a [`STARTUP_WAIT`](super::run::STARTUP_WAIT) spent
/// waiting out a predecessor's shutdown plus `hyperd`'s own start.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(20);

/// Maximum time to wait, after sending `STOP`, for the old daemon to let go of
/// the daemon lock. The lock is released last, after `hyperd` has exited
/// (`HyperProcess`'s drop takes up to about 5 s), so a free lock means a
/// successor can start without colliding with it.
pub const STOP_WAIT: Duration = Duration::from_secs(15);

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
    ensure_daemon_in(&dir, spawn_detached, SPAWN_TIMEOUT, STOP_WAIT)
}

/// [`ensure_daemon`] for an explicit state directory, spawner and wait budget.
fn ensure_daemon_in(
    dir: &Path,
    spawn: impl FnOnce() -> io::Result<()>,
    timeout: Duration,
    stop_wait: Duration,
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
            return maybe_take_over(info, dir, spawn, timeout, stop_wait);
        }

        // The state directory may not exist yet; the daemon creates it.
        let lock = match std::fs::metadata(dir) {
            Ok(_) => DaemonLock::try_acquire(dir)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None::<DaemonLock>,
            Err(error) => return Err(error),
        };
        if let Some(lock) = lock {
            // Nothing holds the lock, so no daemon of this release is running
            // and any record left behind is stale. Clean it up while we hold
            // the lock so a daemon cannot start between the check and the
            // removal.
            //
            // A pre-1.0 daemon holds no lock, so it is retired first; if it
            // will not stop, return without spawning (the caller runs in
            // local mode).
            legacy::retire_under_lock(dir, stop_wait)?;
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

/// Wait until nothing holds the daemon lock in `dir`, polling for up to
/// `timeout`. A missing state directory counts as free.
///
/// The daemon releases the lock last, after its record, listener, socket and
/// `hyperd` are gone, so this is the reliable sign that a daemon has fully
/// exited.
///
/// # Errors
/// Returns [`io::ErrorKind::TimedOut`] if the lock is still held after
/// `timeout`, or the error from probing the lock.
pub fn wait_for_lock_release(dir: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match DaemonLock::try_acquire(dir) {
            Ok(Some(lock)) => {
                drop(lock);
                return Ok(());
            }
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the daemon in {} still holds its lock {} seconds after STOP",
                    dir.display(),
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Decide whether to reuse the running daemon or take it over with a newer
/// version. If the client is newer, it sends `STOP` to the old daemon, waits
/// until the old daemon has released the daemon lock (so its `hyperd` is gone
/// too), then spawns a fresh daemon. Otherwise the existing daemon is reused.
///
/// The health endpoint is the same for the old and the new daemon (it is derived
/// from the state directory), so a fresh daemon is told apart by its pid and
/// start time, and the lock, not the endpoint, says when the old one is gone.
///
/// # Errors
/// If the old daemon still holds the lock after `stop_wait` the takeover gives
/// up with [`io::ErrorKind::TimedOut`] (the caller then runs in local mode)
/// rather than reuse a daemon that is shutting down.
fn maybe_take_over(
    info: DaemonInfo,
    dir: &Path,
    spawn: impl FnOnce() -> io::Result<()>,
    timeout: Duration,
    stop_wait: Duration,
) -> io::Result<DaemonInfo> {
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

    // Wait for the old daemon to let go of the lock.
    if let Err(error) = wait_for_lock_release(dir, stop_wait) {
        warn!(
            endpoint = %info.health_endpoint,
            timeout_secs = stop_wait.as_secs(),
            %error,
            "old daemon did not exit after STOP, not taking over"
        );
        return Err(error);
    }

    // The old daemon is gone, so spawn the new one.
    //
    // Another client could observe the same and spawn concurrently. That is
    // safe: the daemon lock lets exactly one daemon through, and the loser
    // waits for the winner (`run_daemon`) or exits. Narrowing that window: if a
    // concurrent takeover has already published a fresh daemon, adopt it
    // instead of spawning.
    if let Some(fresh) = discovery::discover_in(dir)
        && (fresh.pid != info.pid || fresh.started_at != info.started_at)
    {
        return Ok(fresh);
    }
    spawn()?;
    wait_for_daemon(dir, timeout)
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
            NO_LEGACY_WAIT,
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
            NO_LEGACY_WAIT,
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

        let error = ensure_daemon_in(
            &dir,
            counting_spawner(&calls),
            Duration::from_millis(200),
            NO_LEGACY_WAIT,
        )
        .expect_err("the fake spawner publishes no daemon");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 1, "the spawner must run once");
    }

    /// Legacy wait for tests that have no legacy daemon.
    const NO_LEGACY_WAIT: Duration = Duration::from_millis(100);

    // ─── Retiring a pre-1.0 TCP daemon ──────────────────────────────────

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    /// What the fake legacy daemon does on `STOP`.
    #[derive(Clone, Copy)]
    enum OnStop {
        /// Release the port, as an rc.5 daemon does when it shuts down.
        ClosePort,
        /// Keep answering forever.
        KeepAnswering,
        /// Keep the port open but stop replying (accept, then stay silent).
        GoSilent,
    }

    /// How the fake behaves from the first connection.
    #[derive(Clone, Copy)]
    enum Mode {
        /// An rc.5 daemon reporting this pid.
        Daemon(u32, OnStop),
        /// Accepts, reads the command, never replies.
        Silent,
        /// A foreign service: answers every command with something else.
        Foreign,
    }

    /// An rc.5 look-alike on `127.0.0.1:0`: answers `PING`, `STATUS` and
    /// `STOP`, and records every command it receives.
    struct FakeLegacyDaemon {
        port: u16,
        received: Arc<Mutex<Vec<String>>>,
        quit: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl FakeLegacyDaemon {
        fn start(mode: Mode) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake legacy daemon");
            let port = listener.local_addr().expect("local addr").port();
            listener.set_nonblocking(true).expect("nonblocking");
            let received = Arc::new(Mutex::new(Vec::new()));
            let quit = Arc::new(AtomicBool::new(false));
            let handle = {
                let received = Arc::clone(&received);
                let quit = Arc::clone(&quit);
                std::thread::spawn(move || {
                    let mut silent = matches!(mode, Mode::Silent);
                    while !quit.load(Ordering::SeqCst) {
                        let Ok((stream, _)) = listener.accept() else {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        };
                        stream.set_nonblocking(false).expect("blocking stream");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .expect("read timeout");
                        let mut line = String::new();
                        let mut reader = BufReader::new(stream);
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            continue;
                        }
                        let command = line.trim().to_string();
                        received.lock().expect("received").push(command.clone());
                        let (reported_pid, on_stop) = match mode {
                            Mode::Daemon(pid, on_stop) => (pid, Some(on_stop)),
                            Mode::Silent | Mode::Foreign => (0, None),
                        };
                        if silent {
                            // Hold the connection open without replying.
                            for _ in 0..70 {
                                if quit.load(Ordering::SeqCst) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            continue;
                        }
                        let reply = match (mode, command.as_str()) {
                            (Mode::Foreign, _) => "SSH-2.0-OpenSSH_9.0\n".to_string(),
                            (_, "PING") => "PONG hyperdb-mcp 0.7.0\n".to_string(),
                            (_, "STATUS") => format!("{{\"pid\":{reported_pid}}}\n"),
                            (_, "STOP") => "STOPPING\n".to_string(),
                            _ => "ERR unknown command\n".to_string(),
                        };
                        let _ = reader.get_mut().write_all(reply.as_bytes());
                        if command == "STOP" {
                            match on_stop {
                                Some(OnStop::ClosePort) => break,
                                Some(OnStop::GoSilent) => silent = true,
                                _ => {}
                            }
                        }
                    }
                    // Dropping the listener releases the port.
                })
            };
            Self {
                port,
                received,
                quit,
                handle: Some(handle),
            }
        }

        fn received(&self) -> Vec<String> {
            self.received.lock().expect("received").clone()
        }
    }

    impl Drop for FakeLegacyDaemon {
        fn drop(&mut self) {
            self.quit.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn write_legacy_record(dir: &Path, pid: u32, port: u16) -> std::path::PathBuf {
        let path = dir.join("daemon.json");
        let record = serde_json::json!({
            "pid": pid,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_port": port,
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0",
        });
        std::fs::write(&path, serde_json::to_vec(&record).expect("serialize")).expect("write");
        path
    }

    fn stops(received: &[String]) -> usize {
        received.iter().filter(|command| *command == "STOP").count()
    }

    /// The recorded daemon proves its pid, is stopped, its port goes quiet,
    /// the legacy record is removed, and the new daemon is spawned.
    #[test]
    fn a_legacy_daemon_is_retired_and_the_record_removed() {
        let dir = tempfile::tempdir().expect("state dir");
        let peer = FakeLegacyDaemon::start(Mode::Daemon(4_242, OnStop::ClosePort));
        let record = write_legacy_record(dir.path(), 4_242, peer.port);
        let calls = Cell::new(0);

        let error = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(200),
            Duration::from_secs(5),
        )
        .expect_err("the fake spawner publishes no daemon");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 1, "a retired legacy daemon is replaced");
        assert!(!record.exists(), "the legacy record must be gone");
        let received = peer.received();
        assert_eq!(&received[..3], ["PING", "STATUS", "STOP"], "{received:?}");
        assert_eq!(stops(&received), 1);
    }

    /// A pid that does not match the record means the port belongs to
    /// someone else: no `STOP` is sent, the stale record is removed.
    #[test]
    fn a_legacy_record_with_a_different_pid_gets_no_stop() {
        let dir = tempfile::tempdir().expect("state dir");
        let peer = FakeLegacyDaemon::start(Mode::Daemon(9_999, OnStop::ClosePort));
        let record = write_legacy_record(dir.path(), 4_242, peer.port);
        let calls = Cell::new(0);

        let error = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(200),
            Duration::from_secs(5),
        )
        .expect_err("the fake spawner publishes no daemon");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 1);
        assert!(!record.exists(), "the stale legacy record must be removed");
        let received = peer.received();
        assert_eq!(received, ["PING", "STATUS"], "nothing beyond STATUS");
    }

    /// A legacy record whose port nobody serves is stale: removed, then spawn.
    #[test]
    fn a_legacy_record_with_a_dead_port_is_removed_and_spawns() {
        let dir = tempfile::tempdir().expect("state dir");
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let record = write_legacy_record(dir.path(), 4_242, port);
        let calls = Cell::new(0);

        let _ = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(200),
            Duration::from_secs(5),
        );

        assert_eq!(calls.get(), 1);
        assert!(!record.exists());
    }

    /// A legacy daemon that keeps answering after `STOP`: wait the injected
    /// time, never spawn, leave its record, and report local mode's cause.
    #[test]
    fn a_legacy_daemon_that_never_stops_means_no_spawn() {
        let dir = tempfile::tempdir().expect("state dir");
        let peer = FakeLegacyDaemon::start(Mode::Daemon(4_242, OnStop::KeepAnswering));
        let record = write_legacy_record(dir.path(), 4_242, peer.port);
        let calls = Cell::new(0);

        let started = Instant::now();
        let error = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(200),
            Duration::from_millis(400),
        )
        .expect_err("a legacy daemon that will not stop must not succeed");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("pre-1.0"), "{error}");
        assert_eq!(calls.get(), 0, "the spawner must never run");
        assert!(record.exists(), "the running daemon's record is kept");
        assert!(started.elapsed() >= Duration::from_millis(400));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(stops(&peer.received()), 1, "STOP is sent once");
    }

    /// An open port that never answers is not proof of a dead daemon: keep
    /// the record, send nothing, do not spawn.
    #[test]
    fn a_legacy_port_that_never_replies_keeps_the_record_and_never_spawns() {
        let dir = tempfile::tempdir().expect("state dir");
        let peer = FakeLegacyDaemon::start(Mode::Silent);
        let record = write_legacy_record(dir.path(), 4_242, peer.port);
        let calls = Cell::new(0);

        let error = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(200),
            Duration::from_millis(400),
        )
        .expect_err("an unresponsive legacy port must not succeed");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("pre-1.0"), "{error}");
        assert_eq!(calls.get(), 0, "the spawner must never run");
        assert!(record.exists(), "the record must be kept");
        let received = peer.received();
        assert_eq!(received, ["PING"], "nothing beyond the PING");
    }

    /// A foreign service answering something else holds the port: the record
    /// is stale, removed, and only the PING was sent.
    #[test]
    fn a_foreign_service_on_the_legacy_port_gets_only_a_ping() {
        let dir = tempfile::tempdir().expect("state dir");
        let peer = FakeLegacyDaemon::start(Mode::Foreign);
        let record = write_legacy_record(dir.path(), 4_242, peer.port);
        let calls = Cell::new(0);

        let _ = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(200),
            Duration::from_secs(5),
        );

        assert_eq!(calls.get(), 1);
        assert!(!record.exists(), "the stale record must be removed");
        assert_eq!(peer.received(), ["PING"]);
    }

    /// After `STOP`, a port that hangs (open, silent) still counts as alive.
    #[test]
    fn a_legacy_port_that_hangs_after_stop_counts_as_alive() {
        let dir = tempfile::tempdir().expect("state dir");
        let peer = FakeLegacyDaemon::start(Mode::Daemon(4_242, OnStop::GoSilent));
        let record = write_legacy_record(dir.path(), 4_242, peer.port);
        let calls = Cell::new(0);

        let error = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(200),
            Duration::from_millis(400),
        )
        .expect_err("a hung legacy daemon must not succeed");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("still running"), "{error}");
        assert_eq!(calls.get(), 0, "the spawner must never run");
        assert!(record.exists());
        assert_eq!(stops(&peer.received()), 1);
    }

    // ─── Takeover waits on the lock ─────────────────────────────────────

    use crate::daemon::health::{DaemonState, HealthListener};

    /// A stand-in for an old daemon: holds the lock, serves the health
    /// channel, and on `STOP` removes its record, closes the listener and
    /// (optionally) releases the lock after a pause, as the real one does
    /// while `hyperd` winds down.
    struct FakeOldDaemon {
        state: Arc<DaemonState>,
        quit: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeOldDaemon {
        fn start(dir: &Path, release_after: Option<Duration>) -> Self {
            let lock = DaemonLock::try_acquire(dir)
                .expect("lock probe")
                .expect("fresh dir is lockable");
            let endpoint = HealthEndpoint::for_new_daemon(dir).expect("derive endpoint");
            let listener = HealthListener::bind(&endpoint).expect("bind fake health listener");
            let info = DaemonInfo {
                pid: 1,
                hyperd_endpoint: "127.0.0.1:1".to_string(),
                health_endpoint: endpoint.as_str().to_string(),
                started_at: "2026-01-01T00:00:00Z".to_string(),
                version: "0.0.1".to_string(),
            };
            let record = dir.join("daemon.json");
            std::fs::write(&record, serde_json::to_vec(&info).expect("serialize")).expect("write");

            let state = Arc::new(DaemonState::new(endpoint));
            let quit = Arc::new(AtomicBool::new(false));
            let listener_state = Arc::clone(&state);
            let listener_thread = std::thread::spawn(move || {
                listener.run(listener_state, Arc::new(Mutex::new(info)));
            });
            let watch_state = Arc::clone(&state);
            let watch_quit = Arc::clone(&quit);
            let handle = std::thread::spawn(move || {
                while !watch_state.should_shutdown() && !watch_quit.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = std::fs::remove_file(&record);
                let _ = listener_thread.join();
                match release_after {
                    Some(pause) => std::thread::sleep(pause),
                    None => {
                        while !watch_quit.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                }
                drop(lock);
            });
            Self {
                state,
                quit,
                handle: Some(handle),
            }
        }
    }

    impl Drop for FakeOldDaemon {
        fn drop(&mut self) {
            self.quit.store(true, Ordering::SeqCst);
            self.state.request_shutdown();
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// The takeover must not spawn while the old daemon still holds the lock
    /// (its `hyperd` is still exiting). Red if the lock wait is removed: the
    /// spawner would then see the lock held.
    #[test]
    fn takeover_spawns_only_after_the_old_daemon_released_the_lock() {
        let dir = tempfile::tempdir().expect("state dir");
        let _old = FakeOldDaemon::start(dir.path(), Some(Duration::from_millis(500)));
        let calls = Cell::new(0);
        let lock_was_free = Cell::new(false);
        let started = Instant::now();

        let error = ensure_daemon_in(
            dir.path(),
            || {
                calls.set(calls.get() + 1);
                // Probing and dropping the lock here is the check itself.
                lock_was_free.set(
                    DaemonLock::try_acquire(dir.path())
                        .expect("lock probe")
                        .is_some(),
                );
                Ok(())
            },
            Duration::from_millis(200),
            Duration::from_secs(10),
        )
        .expect_err("the fake spawner publishes no daemon");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 1);
        assert!(
            lock_was_free.get(),
            "spawned while the old daemon held the lock"
        );
        assert!(started.elapsed() >= Duration::from_millis(500));
    }

    /// An old daemon that never releases the lock: no spawn, a `TimedOut`
    /// error (the caller runs in local mode).
    #[test]
    fn takeover_gives_up_without_spawning_if_the_lock_stays_held() {
        let dir = tempfile::tempdir().expect("state dir");
        let _old = FakeOldDaemon::start(dir.path(), None);
        let calls = Cell::new(0);

        let error = ensure_daemon_in(
            dir.path(),
            counting_spawner(&calls),
            Duration::from_millis(200),
            Duration::from_millis(400),
        )
        .expect_err("a daemon that keeps the lock must not be replaced");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls.get(), 0, "the spawner must never run");
    }

    #[test]
    fn wait_for_lock_release_times_out_while_held_and_accepts_a_missing_dir() {
        let dir = tempfile::tempdir().expect("state dir");
        let held = DaemonLock::try_acquire(dir.path())
            .expect("lock probe")
            .expect("fresh dir is lockable");
        let error = wait_for_lock_release(dir.path(), Duration::from_millis(200))
            .expect_err("a held lock must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        drop(held);
        wait_for_lock_release(dir.path(), Duration::from_millis(200)).expect("free lock");
        wait_for_lock_release(&dir.path().join("missing"), Duration::from_millis(200))
            .expect("a missing directory has nothing to wait for");
    }
}
