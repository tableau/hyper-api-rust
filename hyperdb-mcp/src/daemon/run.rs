// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Daemon main loop: spawns `hyperd`, runs health listener, monitors hyperd liveness
//! and optional idle timeout, restarts hyperd if it dies.
//!
//! By default, the daemon never auto-shuts down due to inactivity (opt-in via
//! `--idle-timeout` flag or `HYPERDB_DAEMON_IDLE_TIMEOUT` env var). When enabled,
//! client HEARTBEAT commands reset the idle timer (see [`DaemonState`]).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::signal;
use tracing::{error, info, warn};

use hyperdb_api::{HyperProcess, Parameters, TransportMode};

use super::discovery::{self, DaemonInfo};
use super::health::{DaemonState, HealthListener};
use super::ENV_IDLE_TIMEOUT;

/// Configuration for the daemon process.
#[derive(Debug)]
pub struct DaemonConfig {
    pub port: u16,
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
    pub fn from_args(port: u16, idle_timeout_secs: Option<u64>) -> Self {
        let idle_timeout = idle_timeout_secs
            .or_else(|| {
                std::env::var(ENV_IDLE_TIMEOUT)
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .map(Duration::from_secs);

        Self { port, idle_timeout }
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

/// Run the daemon. This function blocks until shutdown is triggered.
///
/// # Errors
/// Returns an error if the health port cannot be bound, `hyperd` fails to start,
/// or the discovery file cannot be written.
pub async fn run_daemon(config: DaemonConfig) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Bind health port (single-instance lock)
    let listener = HealthListener::bind(config.port).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!(
                "Another hyperdb daemon is already running on port {}. \
                 Use `hyperdb-mcp daemon status` to check or `hyperdb-mcp daemon stop` to stop it.",
                config.port
            )
        } else {
            format!("Failed to bind health port {}: {e}", config.port)
        }
    })?;
    let bound_port = listener.port;
    info!(port = bound_port, "daemon health listener bound");

    // Step 2: Spawn hyperd with TCP transport
    let hyper = HyperProcess::new(None, Some(&build_params()?))?;
    let endpoint = hyper
        .endpoint()
        .ok_or("hyperd did not report an endpoint")?
        .to_string();
    info!(endpoint = %endpoint, "hyperd started");

    // Step 3: Build DaemonInfo and write discovery file
    let info = DaemonInfo {
        pid: std::process::id(),
        hyperd_endpoint: endpoint.clone(),
        health_port: bound_port,
        started_at: chrono::Utc::now().to_rfc3339(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    discovery::write_discovery_file(&info)?;
    info!(path = %discovery::discovery_file_path()?.display(), "discovery file written");

    // Step 4: Build the shared state.
    // - `info_arc` is shared with the health listener so STATUS reports the
    //   current endpoint even after a restart.
    // - `hyper_state` is owned by the monitor task; the listener never touches it.
    let state = Arc::new(DaemonState::new());
    let info_arc = Arc::new(Mutex::new(info));
    let hyper_state = Arc::new(Mutex::new(HyperState {
        hyper: Some(hyper),
        restart_history: Vec::new(),
    }));

    // Step 5: Start health listener in a background thread
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

    // Step 6: Run the three monitors concurrently. Whichever completes first
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
    }
    state.request_shutdown();

    // Step 7: Graceful shutdown.
    // `tokio::select!` already cancelled the monitor and idle-monitor futures
    // when one branch completed, releasing their `hyper_state` Arc clones.
    // When this function returns, the last Arc drops, which drops the inner
    // `HyperState`, which drops the `HyperProcess`, which closes the callback
    // connection and lets hyperd exit cleanly. We don't lock-and-clear here
    // because that would gain nothing — the same drop happens via Arc refcount.
    info!("shutting down daemon");
    discovery::remove_discovery_file();
    let _ = health_handle.join();
    drop(hyper_state); // explicit ordering: drop after health-listener join

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
fn build_params() -> std::io::Result<Parameters> {
    let log_dir = discovery::state_dir()?.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let mut params = Parameters::new();
    params.set("log_file_max_count", "2");
    params.set("log_file_size_limit", "100M");
    params.set("log_dir", log_dir.to_string_lossy().as_ref());
    params.set_transport_mode(TransportMode::Tcp);
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
            let process_dead = guard.hyper.as_mut().map_or(true, HyperProcess::has_exited);
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
/// updates `DaemonInfo`, and rewrites the discovery file.
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
    guard.hyper = None;

    // Spawn the replacement.
    let params = build_params().map_err(|e| RestartError::SpawnFailed(e.to_string()))?;
    let new_hyper = HyperProcess::new(None, Some(&params))
        .map_err(|e| RestartError::SpawnFailed(e.to_string()))?;
    let new_endpoint = new_hyper
        .endpoint()
        .ok_or_else(|| RestartError::SpawnFailed("hyperd did not report endpoint".into()))?
        .to_string();

    // Publish the new endpoint to STATUS readers and to the discovery file.
    // We snapshot the updated DaemonInfo while holding info_arc's lock, then
    // write the file outside the lock to keep the critical section small.
    let snapshot = {
        let mut info_guard = info_arc.lock().expect("DaemonInfo mutex poisoned");
        info_guard.hyperd_endpoint.clone_from(&new_endpoint);
        info_guard.clone()
    };
    discovery::write_discovery_file(&snapshot)
        .map_err(|e| RestartError::SpawnFailed(format!("discovery write: {e}")))?;

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
