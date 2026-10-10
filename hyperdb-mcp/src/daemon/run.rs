// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Daemon main loop: takes the single-instance lock, spawns `hyperd`, runs the
//! health listener, monitors hyperd liveness and optional idle timeout,
//! restarts hyperd if it dies.
//!
//! Startup order: create and check the state directory, take the
//! [`DaemonLock`], bind the control endpoint, start `hyperd`, write
//! `daemon.json`. Shutdown runs it backwards: remove the record, stop the
//! listener (which removes the socket), drop `hyperd`, and only then release
//! the lock. The lock is what a client waits on, so it must outlive everything
//! the next daemon could collide with.
//!
//! By default, the daemon never auto-shuts down due to inactivity (opt-in via
//! `--idle-timeout` flag or `HYPERDB_DAEMON_IDLE_TIMEOUT` env var). When enabled,
//! client HEARTBEAT commands reset the idle timer (see [`DaemonState`]).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::signal;
use tracing::{error, info, warn};

use hyperdb_api::{HyperProcess, Parameters, TransportMode};

use super::ENV_IDLE_TIMEOUT;
use super::control::HealthEndpoint;
use super::discovery::{self, DaemonInfo};
use super::health::{DaemonState, HealthListener};
use super::lock::DaemonLock;

/// How long startup keeps retrying for the daemon lock and for `hyperd`'s
/// socket to come free. A predecessor that was just told to stop needs a moment
/// to let go of both, and a fresh daemon should wait it out rather than fail.
pub const STARTUP_WAIT: Duration = Duration::from_secs(10);

/// Pause between startup retries.
const STARTUP_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// A live daemon must answer for this long before startup believes it is
/// really running (and not merely finishing a shutdown) and gives up.
const ALREADY_RUNNING_DEBOUNCE: Duration = Duration::from_secs(1);

/// Configuration for the daemon process.
#[derive(Debug)]
pub struct DaemonConfig {
    /// Idle timeout duration. When `None`, the daemon never auto-shuts down due to
    /// inactivity (default behavior). When `Some`, the daemon shuts down after the
    /// specified duration without client activity.
    pub idle_timeout: Option<Duration>,
}

impl DaemonConfig {
    /// Construct a `DaemonConfig` from CLI args and environment variables.
    ///
    /// Idle-timeout resolution:
    /// 1. If `idle_timeout_secs` is `Some`, use that value.
    /// 2. Otherwise, if `HYPERDB_DAEMON_IDLE_TIMEOUT` env var is set and parseable, use it.
    /// 3. Otherwise, `None` (never auto-shutdown).
    pub fn from_args(idle_timeout_secs: Option<u64>) -> Self {
        let idle_timeout = idle_timeout_secs
            .or_else(|| {
                std::env::var(ENV_IDLE_TIMEOUT)
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .map(Duration::from_secs);

        Self { idle_timeout }
    }
}

/// Restart-attempt rate limit: at most 3 attempts within this window.
/// The 4th attempt within the window is rejected and triggers daemon shutdown.
pub const RESTART_WINDOW: Duration = Duration::from_secs(60);
pub const RESTART_LIMIT: usize = 3;

/// Polling interval for the hyperd-liveness monitor.
const HYPERD_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// State the monitor task mutates and the main task drops on shutdown.
/// Holds the live `HyperProcess` and the rolling restart-attempt history.
struct HyperState {
    hyper: Option<HyperProcess>,
    restart_history: Vec<Instant>,
}

#[derive(Debug)]
enum RestartError {
    /// More than `RESTART_LIMIT` restart attempts within `RESTART_WINDOW`.
    TooManyRestarts,
    /// `HyperProcess::new` failed or the new process produced no endpoint.
    SpawnFailed(String),
}

/// Take the daemon lock, retrying for up to `deadline`.
///
/// A held lock means another daemon is running, starting, or stopping. Only a
/// daemon that keeps answering an identified `PING` for
/// [`ALREADY_RUNNING_DEBOUNCE`] counts as running: one that is mid-shutdown
/// stops answering within that time and the lock frees up behind it.
async fn acquire_lock(state_dir: &Path, deadline: Instant) -> Result<DaemonLock, String> {
    let mut seen_alive_since: Option<Instant> = None;
    loop {
        match DaemonLock::try_acquire(state_dir) {
            Ok(Some(lock)) => return Ok(lock),
            Ok(None) => {}
            Err(error) => {
                return Err(format!(
                    "Failed to take the daemon lock in {}: {error}",
                    state_dir.display()
                ));
            }
        }

        if discovery::discover_in(state_dir).is_some() {
            let since = *seen_alive_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= ALREADY_RUNNING_DEBOUNCE {
                return Err("Another hyperdb daemon is already running. \
                     Use `hyperdb-mcp daemon status` to check or `hyperdb-mcp daemon stop` to stop it."
                    .to_string());
            }
        } else {
            seen_alive_since = None;
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "Timed out after {}s waiting for another hyperdb daemon in {} to release \
                 the daemon lock",
                STARTUP_WAIT.as_secs(),
                state_dir.display()
            ));
        }
        tokio::time::sleep(STARTUP_RETRY_INTERVAL).await;
    }
}

/// [`build_params`], retrying while another `hyperd` still holds the socket.
/// The previous daemon's `hyperd` can outlive its lock by a moment on some
/// platforms; the pre-flight failing with `AddrInUse` is the signal to wait.
async fn build_params_waiting(state_dir: &Path, deadline: Instant) -> std::io::Result<Parameters> {
    loop {
        match build_params(state_dir) {
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
            {
                tokio::time::sleep(STARTUP_RETRY_INTERVAL).await;
            }
            other => return other,
        }
    }
}

/// Run the daemon. This function blocks until shutdown is triggered.
///
/// # Errors
/// Returns an error if the state directory is not private to the current user,
/// another daemon holds the lock, the control endpoint cannot be bound,
/// `hyperd` fails to start, or the discovery file cannot be written.
pub async fn run_daemon(config: DaemonConfig) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + STARTUP_WAIT;

    // Step 1: Create the state directory, refuse one that others can write to,
    // and take the single-instance lock.
    let state_dir = discovery::state_dir()?;
    super::state_perms::ensure_owner_only_dir(&state_dir)?;
    super::state_perms::verify_state_dir_trusted(&state_dir)?;
    let lock = acquire_lock(&state_dir, deadline).await?;

    // Step 2: Bind the control endpoint. We hold the lock, so anything still
    // sitting at the socket path is a dead daemon's leftover.
    let endpoint = HealthEndpoint::for_new_daemon(&state_dir)?;
    let listener = HealthListener::bind(&endpoint)
        .map_err(|e| format!("Failed to bind the health endpoint {endpoint}: {e}"))?;
    info!(endpoint = %endpoint, "daemon health listener bound");

    // Step 3: Spawn hyperd over a local IPC channel (Unix domain socket on
    // Unix/macOS, named pipe on Windows).
    let hyper = HyperProcess::new(
        None,
        Some(&build_params_waiting(&state_dir, deadline).await?),
    )?;
    // Publish the *connection* endpoint, not the raw callback descriptor. For a
    // Unix domain socket the raw `endpoint()` string reconstructs the path as
    // `<dir>/domain/hyper` (a `tab.domain://` scheme artifact), whereas hyperd
    // actually binds `<dir>/hyper`; `connection_endpoint_string()` carries the path a
    // client can connect to. For TCP and Windows named pipes the two agree.
    let hyperd_endpoint = hyper
        .connection_endpoint_string()
        .ok_or("hyperd did not report a connection endpoint")?;
    info!(endpoint = %hyperd_endpoint, "hyperd started");

    // Step 4: Build DaemonInfo and write discovery file
    let info = DaemonInfo {
        pid: std::process::id(),
        hyperd_endpoint,
        health_endpoint: endpoint.as_str().to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    discovery::write_enriched_discovery_file(&info)?;
    info!(path = %discovery::discovery_file_path()?.display(), "discovery file written");

    // Step 5: Build the shared state.
    // - `info_arc` is shared with the health listener so STATUS reports the
    //   current endpoint even after a restart.
    // - `hyper_state` is owned by the monitor task; the listener never touches it.
    let state = Arc::new(DaemonState::new(endpoint));
    let info_arc = Arc::new(Mutex::new(info));
    let hyper_state = Arc::new(Mutex::new(HyperState {
        hyper: Some(hyper),
        restart_history: Vec::new(),
    }));

    // Step 6: Start health listener in a background thread
    let health_state = Arc::clone(&state);
    let health_info = Arc::clone(&info_arc);
    let health_handle = std::thread::spawn(move || {
        listener.run(health_state, health_info);
    });

    // Log idle-timeout status at startup
    if let Some(d) = config.idle_timeout {
        info!(idle_timeout_secs = d.as_secs(), "idle shutdown enabled");
    } else {
        info!("idle shutdown disabled (daemon will stay resident)");
    }

    // Step 7: Run the three monitors concurrently. Whichever completes first
    // triggers shutdown. If `idle_timeout` is `None`, the idle monitor never fires.
    let idle_fut = async {
        match config.idle_timeout {
            Some(d) => idle_monitor(Arc::clone(&state), d).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        () = idle_fut => {}
        () = hyperd_monitor(Arc::clone(&state), Arc::clone(&hyper_state), Arc::clone(&info_arc)) => {}
        () = shutdown_signal() => {
            info!("received shutdown signal");
        }
        // A `STOP` from a client: act now rather than at the next 5 s monitor
        // poll, so a successor waiting on the lock is not kept waiting.
        () = state.shutdown_requested() => {
            info!("shutdown requested");
        }
    }
    state.request_shutdown();

    // Step 8: Graceful shutdown.
    // `tokio::select!` already cancelled the monitor and idle-monitor futures
    // when one branch completed, releasing their `hyper_state` Arc clones.
    // When this function returns, the last Arc drops, which drops the inner
    // `HyperState`, which drops the `HyperProcess`, which closes the callback
    // connection and lets hyperd exit cleanly. We don't lock-and-clear here
    // because that would gain nothing — the same drop happens via Arc refcount.
    info!("shutting down daemon");
    discovery::remove_discovery_file();
    let _ = health_handle.join();
    // The listener's drop removed the socket when `run` returned. Drop hyperd
    // next, and release the lock last: it is what the next daemon waits on.
    drop(hyper_state); // explicit ordering: drop after health-listener join
    drop(lock);

    Ok(())
}

/// Outcome of a single rate-limit check on the restart-history vector.
#[derive(Debug, PartialEq, Eq)]
pub enum RestartAttempt {
    /// The attempt is within the allowed budget; recorded.
    Recorded,
    /// `RESTART_LIMIT` attempts already happened in the current window.
    LimitExceeded,
}

/// Prune restart-history entries older than `RESTART_WINDOW`, then either
/// record `now` as a new attempt or report that the limit is already
/// exceeded.
///
/// Pulled out as a standalone function so the rate-limit policy can be
/// tested directly without spinning up a real daemon.
pub fn try_record_restart_attempt(history: &mut Vec<Instant>, now: Instant) -> RestartAttempt {
    history.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
    if history.len() >= RESTART_LIMIT {
        return RestartAttempt::LimitExceeded;
    }
    history.push(now);
    RestartAttempt::Recorded
}

/// Build the Parameters used for every hyperd spawn (initial start and restarts).
///
/// `state_dir` is the resolved daemon state directory (from
/// [`discovery::state_dir`]). Taking it as an argument rather than resolving it
/// internally keeps this unit-testable without touching the process
/// environment.
fn build_params(state_dir: &Path) -> std::io::Result<Parameters> {
    // The state directory holds `daemon.json`; `logs/` holds `hyperd`'s own
    // diagnostic logs, which name the endpoint just as `daemon.json` does.
    // `hyperd` is a separate process writing under its own umask, so
    // restricting the directory is what covers those files. Both levels are
    // restricted here so the daemon's own startup establishes the invariant
    // instead of it depending on the later discovery-file write.
    super::state_perms::ensure_owner_only_dir(state_dir)?;
    let log_dir = state_dir.join("logs");
    super::state_perms::ensure_owner_only_dir(&log_dir)?;

    let mut params = Parameters::new();
    params.set("log_file_max_count", "2");
    params.set("log_file_size_limit", "100M");
    params.set("log_dir", log_dir.to_string_lossy().as_ref());

    // The daemon reaches its engine over a local IPC channel rather than TCP.
    params.set_transport_mode(TransportMode::Ipc);

    // On Unix the socket lives in a directory created private to the owner
    // *before* hyperd binds inside it. hyperd binds the socket and never
    // widens its directory, and creating the directory `0700` up front leaves
    // no window in which a client-side tightening would race that bind. On
    // Windows the transport is a named pipe with no directory to place, so
    // this block compiles out (`domain_socket_directory` is Unix-only).
    #[cfg(unix)]
    {
        // `HyperProcess` tracks whether it created the socket directory
        // (`owns_socket_directory`) and only removes one it created, so a
        // caller-supplied `sockets` directory is never deleted on drop.
        let socket_dir = state_dir.join("sockets");
        super::state_perms::ensure_owner_only_dir(&socket_dir)?;

        // Fast-fail pre-flight. hyperd binds `<socket_dir>/hyper`; if another
        // hyperd is *genuinely* bound there the connect succeeds, and without
        // this guard hyperd would die with "unable to listen on domain socket:
        // domain socket is in use" while `HyperProcess::new` masked it as a
        // 60-second "Timeout waiting for Hyper to connect to callback listener".
        // Reachable whenever a previous daemon's hyperd has not exited yet:
        // a takeover or a manual start right after a `STOP`, or two daemons
        // racing for the same state directory (`build_params_waiting` retries
        // on this within `STARTUP_WAIT`). A refused/missing socket (stale file,
        // dead owner) is fine: proceed and let hyperd's pid-liveness staleness
        // check reclaim it. This keeps the crash-restart path safe:
        // `try_restart_hyperd` reaps the SIGKILLed child (`guard.hyper = None`)
        // before calling `build_params`, so there the connect is refused and
        // we proceed to rebind.
        let socket_path = socket_dir.join("hyper");
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "another hyperd is already bound at {}",
                    socket_path.display()
                ),
            ));
        }

        params.set_domain_socket_directory(socket_dir);
    }

    Ok(params)
}

/// Watches for idle timeout. Triggers shutdown when no activity for
/// `idle_timeout`. Wakes every 10 seconds.
async fn idle_monitor(state: Arc<DaemonState>, idle_timeout: Duration) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        if state.should_shutdown() {
            return;
        }
        if state.idle_duration() >= idle_timeout {
            info!(
                idle_secs = idle_timeout.as_secs(),
                "idle timeout reached, shutting down"
            );
            return;
        }
    }
}

/// Watches hyperd's liveness. If hyperd has exited (or a client reported it as
/// dead), restarts it and rewrites the discovery file. If restarts exceed the
/// rate limit, returns and lets the main task initiate shutdown.
async fn hyperd_monitor(
    state: Arc<DaemonState>,
    hyper_state: Arc<Mutex<HyperState>>,
    info_arc: Arc<Mutex<DaemonInfo>>,
) {
    loop {
        tokio::time::sleep(HYPERD_POLL_INTERVAL).await;
        if state.should_shutdown() {
            return;
        }

        // Check liveness and consume the restart-request flag *atomically* with
        // the decision: we only swap-to-false when we're actually committing to
        // act, so a flag set after this point survives to the next tick.
        let needs_restart = {
            let mut guard = hyper_state.lock().expect("HyperState mutex poisoned");
            let process_dead = guard.hyper.as_mut().is_none_or(HyperProcess::has_exited);
            // Only consume the flag when we're going to restart anyway, OR
            // when the process is alive and we want to honor a client report.
            if process_dead {
                // Drain the flag so a stale post-death report doesn't cause a
                // spurious double-restart on the next tick.
                let _ = state.consume_restart_request();
                true
            } else {
                state.consume_restart_request()
            }
        };

        if !needs_restart {
            continue;
        }

        match try_restart_hyperd(&hyper_state, &info_arc) {
            Ok(new_endpoint) => {
                info!(endpoint = %new_endpoint, "hyperd restarted");
                // Drain any reports that landed *during* the restart — those
                // clients were complaining about the now-replaced hyperd, not
                // the freshly spawned one. Without this, the next tick would
                // see an alive process + a stale flag and trigger a spurious
                // double-restart.
                let _ = state.consume_restart_request();
            }
            Err(RestartError::TooManyRestarts) => {
                error!(
                    limit = RESTART_LIMIT,
                    window_secs = RESTART_WINDOW.as_secs(),
                    "hyperd restart limit exceeded — daemon shutting down"
                );
                return;
            }
            Err(RestartError::SpawnFailed(e)) => {
                warn!(error = %e, "hyperd spawn failed during restart; will retry on next tick");
            }
        }
    }
}

/// Attempt one restart of hyperd. Drops the old process, spawns a new one,
/// rewrites the discovery file, and only then updates the `DaemonInfo` that
/// STATUS serves — see the publish-order comment in the body for why that
/// sequence is load-bearing rather than incidental.
///
/// Every call (success or spawn-failure) consumes one slot from the rate-limit
/// window — a broken hyperd binary should not spin forever.
fn try_restart_hyperd(
    hyper_state: &Mutex<HyperState>,
    info_arc: &Mutex<DaemonInfo>,
) -> Result<String, RestartError> {
    let mut guard = hyper_state.lock().expect("HyperState mutex poisoned");

    // Rate-limit check: prune-check-push.
    if try_record_restart_attempt(&mut guard.restart_history, Instant::now())
        == RestartAttempt::LimitExceeded
    {
        return Err(RestartError::TooManyRestarts);
    }

    // Drop the old hyperd. For an already-exited process this is near-instant;
    // for a still-alive process, Drop waits up to ~5s for graceful shutdown.
    //
    // Load-bearing ordering: this drop MUST precede the respawn below. A
    // SIGKILLed hyperd leaves a zombie until its `Child` handle is reaped, and a
    // zombie still looks alive to hyperd's stale-socket check (pid-liveness on
    // `<dir>/hyper.pid`) *and* to `build_params`' pre-flight connect. Dropping
    // the handle here reaps the child so the replacement can reclaim the socket;
    // reordering the respawn before this line wedges the new hyperd for the full
    // 60-second callback timeout.
    guard.hyper = None;

    // Spawn the replacement.
    let state_dir = discovery::state_dir().map_err(|e| RestartError::SpawnFailed(e.to_string()))?;
    let params = build_params(&state_dir).map_err(|e| RestartError::SpawnFailed(e.to_string()))?;
    let new_hyper = HyperProcess::new(None, Some(&params))
        .map_err(|e| RestartError::SpawnFailed(e.to_string()))?;
    // See `run_daemon`: publish the connectable `connection_endpoint_string()`, not
    // the raw callback descriptor, so a Unix socket path is the one hyperd
    // actually bound.
    let new_endpoint = new_hyper.connection_endpoint_string().ok_or_else(|| {
        RestartError::SpawnFailed("hyperd did not report a connection endpoint".into())
    })?;

    // Publish the new endpoint, discovery file first and STATUS second.
    //
    // Two independent channels advertise the endpoint: the `daemon.json`
    // discovery file that `discovery::discover()` reads — the path every
    // client's `Engine::new` takes — and the in-memory `DaemonInfo` that the
    // health channel's STATUS command serves. Updating STATUS first and the file
    // afterwards left a window in which STATUS announced the freshly spawned
    // `hyperd` while the file still named the one we just dropped, so a client
    // discovering during that window connected to a dead port. Persisting
    // first inverts that: whichever channel an observer reads, seeing the new
    // endpoint now implies a live `hyperd` behind it.
    //
    // Both steps happen under a single `info_arc` lock so the pair is atomic
    // to STATUS readers — no reader can catch STATUS already advertising an
    // endpoint the file has not committed yet. The critical section is one
    // small serialize-and-rename, and the only contender is `status_json`,
    // whose own critical section is a single clone.
    // `write_enriched_discovery_file` never touches `info_arc`, so holding it
    // here cannot deadlock.
    //
    // Failing the write before mutating `DaemonInfo` also matters: the `?`
    // below drops `new_hyper` on the way out, and STATUS must not be left
    // advertising the endpoint of a `hyperd` this function just killed.
    {
        let mut info_guard = info_arc.lock().expect("DaemonInfo mutex poisoned");
        let mut snapshot = info_guard.clone();
        snapshot.hyperd_endpoint.clone_from(&new_endpoint);
        discovery::write_enriched_discovery_file(&snapshot)
            .map_err(|e| RestartError::SpawnFailed(format!("discovery write: {e}")))?;
        info_guard.hyperd_endpoint.clone_from(&new_endpoint);
    }

    guard.hyper = Some(new_hyper);
    Ok(new_endpoint)
}

async fn shutdown_signal() {
    let ctrl_c = signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm =
            signal::unix::signal(signal::unix::SignalKind::terminate()).expect("sigterm handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::TempDir;

    use super::*;

    /// The daemon's Unix domain socket lives in a directory it creates private
    /// to the owner *before* hyperd binds inside it. This reads the mode
    /// straight back off the directory `build_params` created and handed to
    /// `HyperProcess` through the `domain_socket_directory` override, so a
    /// regression that dropped the `ensure_owner_only_dir` call — leaving the
    /// directory at its umask-derived mode — would surface here.
    #[test]
    fn build_params_creates_owner_only_socket_directory() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");

        let params = build_params(&state_dir).expect("build_params should create the state layout");

        let socket_dir = params
            .domain_socket_directory()
            .expect("IPC params must carry a Unix domain socket directory");
        assert_eq!(
            socket_dir,
            state_dir.join("sockets"),
            "the socket directory must be a dedicated subdirectory under the state directory"
        );

        let mode = std::fs::metadata(socket_dir)
            .expect("the socket directory must exist on disk")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "the daemon's socket directory must be owner-only before hyperd binds in it, \
             got {mode:04o}"
        );
    }
}
