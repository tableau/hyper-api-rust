// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the single-instance daemon: discovery file, health protocol,
//! idle timeout, the daemon lock, and full lifecycle integration with a real
//! `hyperd`.
//!
//! A daemon is identified by its state directory: the lock is `daemon.lock`,
//! the health channel is the per-user socket `daemon.sock`, and `daemon.json`
//! points at both. Every fake peer here therefore lives in its own short
//! per-test state directory (a Unix socket path must fit in `sun_path`, 104
//! bytes on macOS).
//!
//! Many tests mutate the process-global environment variable
//! `HYPERDB_STATE_DIR` to isolate their state directories. Because env vars
//! are process-global, these tests MUST run sequentially. We enforce this via a
//! shared mutex — every test that touches env vars acquires `ENV_LOCK` first.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyperdb_mcp::daemon::control::{self, ControlListener, ControlStream, HealthEndpoint};
use hyperdb_mcp::daemon::discovery::{self, DaemonInfo};
use hyperdb_mcp::daemon::health::{self, DaemonState, HealthListener};
use hyperdb_mcp::daemon::lock::DaemonLock;
use tempfile::TempDir;

/// Process-wide lock for tests that mutate environment variables.
/// Cargo runs tests in the same process by default — this prevents races.
/// We recover from poison to prevent one test's panic from cascading.
static ENV_LOCK: Mutex<()> = Mutex::new(());
const ENGINE_REPORT_CHILD_ENV: &str = "HYPERDB_MCP_ENGINE_REPORT_CHILD";
const ENGINE_REPORT_TEST_NAME: &str = "report_hyperd_error_targets_discovered_health_endpoint";

/// Seconds a crash-recovery test waits for the daemon to bring a fresh `hyperd`
/// back to a reachable state after the running engine is killed.
///
/// Derivation: the liveness monitor polls on a 5 s tick
/// (`daemon::run::HYPERD_POLL_INTERVAL`), so a kill landing just after a tick
/// costs up to ~5 s just to be *noticed*. Recovery then has to drop the dead
/// process, cold-spawn a new `hyperd`, rebind, and republish STATUS. On a
/// loaded shared CI runner (`ubuntu-latest`) a cold engine spawn alone can eat
/// several seconds, so the previous 12 s budget (~5 s detection + ~7 s slack)
/// tripped as a *false* timeout even though recovery was progressing — these
/// tests pass 4/4 locally and the third restart test passes on the same CI run.
/// 45 s keeps ~40 s of headroom past the detection tick, comfortably absorbing a
/// slow runner while still bounding a genuinely wedged daemon (which would never
/// republish and would fail at the deadline regardless). This is a budget bump,
/// not a coverage change: the restart tests still run on Linux. See issue #305
/// for the standing daemon-test timing decision (which now spans Linux, not just
/// the macOS-ignored set).
///
/// `#[cfg(unix)]` because its only use sites are the `#[cfg(unix)]` restart
/// tests (`hyperd_monitor_detects_killed_hyperd_and_restarts`,
/// `client_report_triggers_restart_after_kill`,
/// `engine_recovers_after_hyperd_killed`) and their Unix-only helpers. Without
/// the gate the constant is dead code on Windows, which `clippy -D warnings`
/// rejects.
#[cfg(unix)]
const RESTART_READINESS_BUDGET_SECS: u64 = 45;

fn acquire_env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ─── Unit tests: DaemonState (no env vars, safe to run in parallel) ───────────

/// A `DaemonState` for a daemon whose endpoint lives in a fresh state
/// directory. The directory must outlive the state.
fn new_daemon_state() -> (TempDir, DaemonState) {
    let (dir, endpoint) = test_endpoint();
    (dir, DaemonState::new(endpoint))
}

#[test]
fn daemon_state_touch_resets_idle_duration() {
    let (_dir, state) = new_daemon_state();
    std::thread::sleep(Duration::from_millis(50));
    assert!(state.idle_duration() >= Duration::from_millis(50));

    state.touch();
    assert!(state.idle_duration() < Duration::from_millis(30));
}

#[test]
fn daemon_state_shutdown_flag() {
    let (_dir, state) = new_daemon_state();
    assert!(!state.should_shutdown());

    state.request_shutdown();
    assert!(state.should_shutdown());
}

#[test]
fn daemon_state_new_initializes_restart_flag_false() {
    let (_dir, state) = new_daemon_state();
    assert!(!state.consume_restart_request());
}

#[test]
fn daemon_state_restart_request_consume_round_trip() {
    let (_dir, state) = new_daemon_state();
    assert!(!state.consume_restart_request(), "initially clear");

    state.request_restart();
    assert!(state.consume_restart_request(), "consume returns true once");
    assert!(
        !state.consume_restart_request(),
        "second consume returns false"
    );

    // Multiple requests coalesce into one consumption.
    state.request_restart();
    state.request_restart();
    state.request_restart();
    assert!(
        state.consume_restart_request(),
        "three requests → one consume"
    );
    assert!(!state.consume_restart_request());
}

#[test]
fn restart_history_records_attempts_under_limit() {
    use hyperdb_mcp::daemon::run::{RestartAttempt, try_record_restart_attempt};
    let mut history: Vec<Instant> = Vec::new();
    let t0 = Instant::now();

    assert_eq!(
        try_record_restart_attempt(&mut history, t0),
        RestartAttempt::Recorded
    );
    assert_eq!(
        try_record_restart_attempt(&mut history, t0 + Duration::from_secs(10)),
        RestartAttempt::Recorded
    );
    assert_eq!(
        try_record_restart_attempt(&mut history, t0 + Duration::from_secs(20)),
        RestartAttempt::Recorded
    );
    assert_eq!(
        history.len(),
        3,
        "first three attempts within window are recorded"
    );
}

#[test]
fn restart_history_rejects_fourth_attempt_in_window() {
    use hyperdb_mcp::daemon::run::{RestartAttempt, try_record_restart_attempt};
    let mut history: Vec<Instant> = Vec::new();
    let t0 = Instant::now();

    // Fill the window with 3 attempts.
    try_record_restart_attempt(&mut history, t0);
    try_record_restart_attempt(&mut history, t0 + Duration::from_secs(10));
    try_record_restart_attempt(&mut history, t0 + Duration::from_secs(20));

    // 4th attempt within the 60s window must be rejected.
    assert_eq!(
        try_record_restart_attempt(&mut history, t0 + Duration::from_secs(30)),
        RestartAttempt::LimitExceeded,
        "4th attempt within window must be rejected"
    );
    assert_eq!(history.len(), 3, "rejection must not push to history");
}

#[test]
fn restart_history_prunes_entries_older_than_window() {
    use hyperdb_mcp::daemon::run::{RestartAttempt, try_record_restart_attempt};
    let mut history: Vec<Instant> = Vec::new();
    let t0 = Instant::now();

    // Three attempts at the start of the timeline.
    try_record_restart_attempt(&mut history, t0);
    try_record_restart_attempt(&mut history, t0 + Duration::from_secs(5));
    try_record_restart_attempt(&mut history, t0 + Duration::from_secs(10));

    // Now jump 70 seconds — all three are stale and should be pruned.
    let later = t0 + Duration::from_secs(70);
    assert_eq!(
        try_record_restart_attempt(&mut history, later),
        RestartAttempt::Recorded,
        "after window expires, restarts are allowed again"
    );
    assert_eq!(
        history.len(),
        1,
        "stale entries pruned, only 'later' remains"
    );
}

// ─── Unit tests: DaemonConfig (require ENV_LOCK) ─────────────────────────────

#[test]
fn daemon_config_from_args_none_when_unset() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::remove("HYPERDB_DAEMON_IDLE_TIMEOUT");

    let config = hyperdb_mcp::daemon::run::DaemonConfig::from_args(None);
    assert!(
        config.idle_timeout.is_none(),
        "idle_timeout should be None when neither flag nor env is set"
    );
}

#[test]
fn daemon_config_from_args_some_when_flag() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::remove("HYPERDB_DAEMON_IDLE_TIMEOUT");

    let config = hyperdb_mcp::daemon::run::DaemonConfig::from_args(Some(120));
    assert_eq!(
        config.idle_timeout,
        Some(Duration::from_secs(120)),
        "idle_timeout should match the provided flag value"
    );
}

#[test]
fn daemon_config_from_args_some_when_env() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::set("HYPERDB_DAEMON_IDLE_TIMEOUT", "90");

    let config = hyperdb_mcp::daemon::run::DaemonConfig::from_args(None);
    assert_eq!(
        config.idle_timeout,
        Some(Duration::from_secs(90)),
        "idle_timeout should match the env var value when no flag is provided"
    );
}

#[test]
fn daemon_config_from_args_flag_takes_precedence() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::set("HYPERDB_DAEMON_IDLE_TIMEOUT", "90");

    let config = hyperdb_mcp::daemon::run::DaemonConfig::from_args(Some(120));
    assert_eq!(
        config.idle_timeout,
        Some(Duration::from_secs(120)),
        "flag value should take precedence over env var"
    );
}

// ─── Unit tests: Health protocol (no env vars, safe to run in parallel) ───────

#[test]
fn health_listener_bind_succeeds_on_a_free_endpoint() {
    let (_dir, endpoint) = test_endpoint();
    let listener = HealthListener::bind(&endpoint).unwrap();
    assert_eq!(listener.endpoint(), &endpoint);
}

#[test]
fn health_listener_second_bind_on_a_live_endpoint_fails() {
    let (_dir, endpoint) = test_endpoint();
    let _first = HealthListener::bind(&endpoint).unwrap();

    let second = HealthListener::bind(&endpoint);
    assert!(
        second.is_err(),
        "a live listener must not be displaced by a second bind"
    );
}

/// The lock, not the socket, is what keeps a state directory to one daemon:
/// it is exclusive per state directory (also within one process, which the
/// in-process test daemons rely on), independent across directories, and
/// released on drop.
#[test]
fn daemon_lock_is_exclusive_per_state_dir_and_released_on_drop() {
    let first_dir = TempDir::new().unwrap();
    let second_dir = TempDir::new().unwrap();

    let held = DaemonLock::try_acquire(first_dir.path())
        .unwrap()
        .expect("a free state directory yields the lock");
    assert!(
        DaemonLock::try_acquire(first_dir.path()).unwrap().is_none(),
        "a second holder in the same state directory must be refused"
    );
    assert!(
        DaemonLock::try_acquire(second_dir.path())
            .unwrap()
            .is_some(),
        "another state directory has its own lock"
    );

    drop(held);
    // A descriptor can linger briefly in a sibling test's freshly forked child
    // (it is close-on-exec, so only until that child's exec), hence the short
    // bounded wait rather than a single probe.
    let deadline = Instant::now() + Duration::from_secs(5);
    let released = loop {
        if DaemonLock::try_acquire(first_dir.path()).unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        released,
        "dropping the lock must free it for the next holder"
    );
}

#[test]
fn health_listener_waits_for_command_after_accept() {
    use std::sync::mpsc;

    const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(1);
    const OVERALL_WATCHDOG: Duration = Duration::from_secs(3);

    let (_dir, mut health) = start_health_listener();
    let endpoint = health.endpoint.clone();
    let (outcome_tx, outcome_rx) = mpsc::sync_channel(1);
    let started = Instant::now();

    let client = std::thread::spawn(move || {
        struct IdleClient(ControlStream);

        impl IdleClient {
            fn connect(
                endpoint: &HealthEndpoint,
                timeout: Duration,
                label: &str,
            ) -> Result<Self, String> {
                let mut stream = control::connect(endpoint, timeout)
                    .map_err(|error| format!("connect {label} raw health client: {error}"))?;
                stream
                    .set_io_timeout(timeout)
                    .map_err(|error| format!("set {label} client I/O timeout: {error}"))?;
                Ok(Self(stream))
            }

            fn ping(&mut self, label: &str) -> Result<String, String> {
                self.0.write_all(b"PING\n").map_err(|error| {
                    format!("{label} PING write failed ({:?}): {error}", error.kind())
                })?;

                match read_line_from(&mut self.0) {
                    Ok(response) if response.is_empty() => Err(format!(
                        "health listener returned early EOF for {label} PING"
                    )),
                    Ok(response) => Ok(response),
                    Err(error) => Err(format!(
                        "{label} PING read failed ({:?}): {error}",
                        error.kind()
                    )),
                }
            }
        }

        let outcome = (|| -> Result<String, String> {
            let expected_pong = pong_line();
            let mut idle_client = IdleClient::connect(&endpoint, CLIENT_IO_TIMEOUT, "first idle")?;

            // A PONG from a later connection proves the listener has accepted
            // and serviced connections while the first one remains byte-empty.
            {
                let mut progress_probe =
                    IdleClient::connect(&endpoint, CLIENT_IO_TIMEOUT, "progress probe")?;
                let probe_response = progress_probe.ping("progress-probe")?;
                if probe_response != expected_pong {
                    return Err(format!(
                        "progress probe returned unidentified response: {probe_response:?}"
                    ));
                }
            }

            // An accepted connection must not inherit the listener's
            // non-blocking mode: a non-blocking read would fail with
            // WouldBlock at once. Keep the first connection idle for 350 ms,
            // well inside the handler's 5 s read timeout, before sending
            // its first command.
            std::thread::sleep(Duration::from_millis(350));
            idle_client.ping("delayed first-client")
        })();
        let _ = outcome_tx.send(outcome);
    });

    let outcome = outcome_rx.recv_timeout(OVERALL_WATCHDOG);
    health.stop_and_join();
    let client_join = client.join();

    assert!(
        client_join.is_ok(),
        "raw health-client thread panicked after listener cleanup"
    );
    let response = outcome.unwrap_or_else(|error| {
        panic!("raw health-client scenario exceeded the {OVERALL_WATCHDOG:?} watchdog: {error}")
    });
    assert_eq!(
        response,
        Ok(pong_line()),
        "HealthListener must keep an accepted connection open until its first command; elapsed={:?}",
        started.elapsed()
    );
}

#[test]
fn health_protocol_ping_pong() {
    let (_dir, health) = start_health_listener();

    let response = health::send_command(&health.endpoint, "PING").unwrap();
    assert!(response.trim().starts_with("PONG hyperdb-mcp "));
}

#[test]
fn health_protocol_heartbeat_resets_idle() {
    let (_dir, health) = start_health_listener();

    std::thread::sleep(Duration::from_millis(50));
    assert!(health.state.idle_duration() >= Duration::from_millis(50));

    let response = health::send_command(&health.endpoint, "HEARTBEAT").unwrap();
    assert_eq!(response.trim(), "OK");

    assert!(health.state.idle_duration() < Duration::from_millis(30));
}

#[test]
fn health_protocol_stop_triggers_shutdown() {
    let (_dir, mut health) = start_health_listener();

    assert!(!health.state.should_shutdown());

    let response = health::send_command(&health.endpoint, "STOP").unwrap();
    assert_eq!(response.trim(), "STOPPING");

    assert!(health.state.should_shutdown());

    // Health listener should exit its loop
    health.join();
}

#[test]
fn health_protocol_status_returns_json() {
    let (_dir, health) = start_health_listener();

    let response = health::send_command(&health.endpoint, "STATUS").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
    assert_eq!(parsed["pid"], 12345);
    assert_eq!(parsed["hyperd_endpoint"], "127.0.0.1:54321");
}

#[test]
fn health_protocol_unknown_command_returns_error() {
    let (_dir, health) = start_health_listener();

    let response = health::send_command(&health.endpoint, "INVALID").unwrap();
    assert!(response.contains("ERR"));
}

#[test]
fn health_protocol_report_hyperd_error_sets_flag() {
    let (_dir, health) = start_health_listener();

    assert!(!health.state.consume_restart_request(), "flag starts clear");

    let response = health::send_command(&health.endpoint, "REPORT_HYPERD_ERROR").unwrap();
    assert_eq!(response.trim(), "OK");

    // The handler ran on a different thread. The flag is set before the OK
    // reply is written, so it is observable as soon as send_command returns.
    assert!(
        health.state.consume_restart_request(),
        "REPORT_HYPERD_ERROR must set the restart-requested flag"
    );
}

#[test]
fn health_protocol_multi_command_session() {
    let (_dir, health) = start_health_listener();

    let response1 = health::send_command(&health.endpoint, "PING").unwrap();
    assert!(response1.trim().starts_with("PONG hyperdb-mcp "));

    let response2 = health::send_command(&health.endpoint, "STATUS").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(response2.trim()).unwrap();
    assert_eq!(parsed["health_endpoint"], health.endpoint.as_str());

    let response3 = health::send_command(&health.endpoint, "HEARTBEAT").unwrap();
    assert_eq!(response3.trim(), "OK");
}

#[test]
fn health_protocol_ping_identity_accept() {
    let (_dir, health) = start_health_listener();

    let version = health::ping_identified(
        &health.endpoint,
        Duration::from_millis(300),
        Duration::from_millis(300),
    )
    .expect("should return Some for a valid hyperdb-mcp daemon");
    assert_eq!(version, hyperdb_mcp::version::MCP_VERSION);
}

#[test]
fn health_protocol_ping_identity_reject_foreign() {
    // A peer that returns a bare "PONG\n" without the identifying token.
    let dir = TempDir::new().unwrap();
    let mut peer = FakeHealthPeer::start_in(dir.path(), |_| b"PONG\n".to_vec());

    let result = health::ping_identified(
        &peer.endpoint,
        Duration::from_millis(300),
        Duration::from_millis(300),
    );
    assert_eq!(result, None, "should reject foreign PONG without token");
    assert_eq!(
        peer.stop_and_join(),
        ["PING"],
        "the peer must actually have been asked, or the rejection proves nothing"
    );
}

#[test]
fn health_protocol_ping_identity_reject_token_lookalike() {
    // A foreign service whose token *starts with* "hyperdb-mcp" must NOT pass.
    // Guards against a naive `starts_with("PONG hyperdb-mcp")` prefix check.
    let dir = TempDir::new().unwrap();
    let mut peer =
        FakeHealthPeer::start_in(dir.path(), |_| b"PONG hyperdb-mcpEVIL 9.9.9\n".to_vec());

    let result = health::ping_identified(
        &peer.endpoint,
        Duration::from_millis(300),
        Duration::from_millis(300),
    );
    assert_eq!(
        result, None,
        "must reject a token that only shares a prefix"
    );
    assert_eq!(peer.stop_and_join(), ["PING"]);
}

#[test]
fn health_protocol_ping_identity_reject_refused() {
    // Bind a listener, then drop it so nothing serves the endpoint any more.
    let (_dir, endpoint) = test_endpoint();
    let listener = ControlListener::bind(&endpoint).unwrap();
    drop(listener);

    let result = health::ping_identified(
        &endpoint,
        Duration::from_millis(300),
        Duration::from_millis(300),
    );
    assert_eq!(result, None, "should return None when nothing serves it");
}

/// The removed `--port` flag. Driven through the real binary because `Cli`
/// lives in `main.rs` and is not reachable from a test crate. clap must reject
/// it as a usage error (exit 2) rather than a daemon quietly accepting it.
///
/// Each child is waited on against a deadline and killed if it outlives it:
/// had `--port` still been accepted, `daemon --port 1234` would run a
/// foreground daemon forever and a plain `Command::output()` would never
/// return.
#[test]
fn daemon_cli_rejects_the_removed_port_flag() {
    let sandbox = TempDir::new().expect("temp state dir for the port-flag CLI test");

    for args in [
        ["daemon", "--port", "1234"].as_slice(),
        ["daemon", "--port", "0"].as_slice(),
        ["daemon", "stop", "--port", "1234"].as_slice(),
        ["daemon", "status", "--port", "1234"].as_slice(),
    ] {
        let output = run_cli_bounded(args, sandbox.path(), Duration::from_secs(20));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: expected clap's usage-error exit 2\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains("unexpected argument"),
            "{args:?}: stderr should say the argument is unexpected, got {stderr:?}"
        );
    }

    assert!(
        !sandbox.path().join("daemon.json").exists(),
        "no invocation in this test may publish a discovery record; \
         a rejected flag must not have started a daemon"
    );
}

#[test]
fn report_hyperd_error_targets_discovered_health_endpoint() {
    let _lock = acquire_env_lock();
    if std::env::var_os(ENGINE_REPORT_CHILD_ENV).is_some() {
        run_engine_report_child();
    } else {
        run_bounded_engine_report_child();
    }
}

fn run_bounded_engine_report_child() {
    let temp_dir = TempDir::new().expect("create isolated engine-construction state root");
    let state_dir = temp_dir.path().join("state");
    let process_temp_dir = temp_dir.path().join("tmp");
    std::fs::create_dir_all(&state_dir).expect("create isolated engine-construction state");
    std::fs::create_dir_all(&process_temp_dir).expect("create isolated process temp directory");

    let mut command = std::process::Command::new(
        std::env::current_exe().expect("locate daemon integration-test binary"),
    );
    command
        .arg("--exact")
        .arg(ENGINE_REPORT_TEST_NAME)
        .arg("--nocapture")
        .current_dir(temp_dir.path())
        .env(ENGINE_REPORT_CHILD_ENV, "1")
        .env("HOME", &state_dir)
        .env("USERPROFILE", &state_dir)
        .env("HYPERDB_STATE_DIR", &state_dir)
        .env("TMPDIR", &process_temp_dir)
        .env("TEMP", &process_temp_dir)
        .env("TMP", &process_temp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .expect("spawn exact Engine-report regression child");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let kill_result = child.kill();
                let output = child
                    .wait_with_output()
                    .expect("reap timed-out Engine-report child");
                panic!(
                    "Engine-report child exceeded 10s; kill={kill_result:?}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error) => {
                let kill_result = child.kill();
                let output = child
                    .wait_with_output()
                    .expect("reap Engine-report child after status error");
                panic!(
                    "Engine-report child status failed: {error}; kill={kill_result:?}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
    let output = child
        .wait_with_output()
        .expect("collect completed Engine-report child output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("running 1 test"),
        "exact child filter executed zero or multiple tests\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "Engine-report child failed with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}

fn run_engine_report_child() {
    let state_dir = discovery::state_dir().expect("child state directory");
    let mut health = RunningHealth::start_with_info(&state_dir, |endpoint| DaemonInfo {
        pid: 616_161,
        // Port zero is never a reachable TCP server endpoint. It makes the
        // public Engine constructor enter the daemon connection-error branch
        // without racing another process for a recently released port.
        hyperd_endpoint: "127.0.0.1:0".to_string(),
        health_endpoint: endpoint.as_str().to_string(),
        started_at: "2026-08-14T12:34:56Z".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    });
    let daemon_info = health
        .info
        .lock()
        .expect("daemon info mutex must not be poisoned")
        .clone();
    discovery::write_discovery_file(&daemon_info).expect("write isolated daemon discovery record");
    assert!(
        !health.state.consume_restart_request(),
        "restart flag must begin clear"
    );

    let error = hyperdb_mcp::engine::Engine::new(None)
        .expect_err("unreachable discovered Hyper endpoint must fail Engine construction");

    let restart_requested = health.state.consume_restart_request();
    health.stop_and_join();
    assert!(
        error
            .message
            .contains("Failed to connect to daemon hyperd at 127.0.0.1:0"),
        "public Engine constructor must fail in try_daemon_mode, got: {}",
        error.message
    );
    assert!(
        restart_requested,
        "Engine::try_daemon_mode must report through the health endpoint from discovery"
    );
}

// ─── Unit tests: idle timeout logic (no env vars) ─────────────────────────────

/// Covers the idle *arithmetic* only — it drives a bare `DaemonState` with its
/// own monitor and never binds a `HealthListener`, so it stays green no matter
/// what the accept loop does. `idle_timeout_stops_a_real_health_listener`
/// below covers the composition this one cannot see.
#[test]
fn daemon_idle_timeout_shuts_down_daemon() {
    let idle_timeout = Duration::from_secs(2);

    // Captured before `DaemonState::new()`, which is what starts the idle
    // countdown. The monitor decides against `last_activity`, so a reference
    // taken after construction measures a shorter interval than the one the
    // daemon actually waited, and the lower bound below would then turn on how
    // long this thread took to get from `new()` to `Instant::now()` rather
    // than on the timeout under test.
    let start = Instant::now();
    let (_dir, state) = new_daemon_state();
    let state = Arc::new(state);
    let monitor_state = Arc::clone(&state);
    let monitor = std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if monitor_state.idle_duration() >= idle_timeout {
                monitor_state.request_shutdown();
                break;
            }
            if monitor_state.should_shutdown() {
                break;
            }
        }
    });

    monitor.join().unwrap();
    let elapsed = start.elapsed();

    assert!(state.should_shutdown());
    assert!(
        elapsed >= idle_timeout,
        "idle shutdown fired after {elapsed:?}, before the {idle_timeout:?} idle period elapsed"
    );
    assert!(
        elapsed < idle_timeout * 2,
        "idle shutdown took {elapsed:?}, far beyond the {idle_timeout:?} timeout"
    );
}

/// The full idle-shutdown composition, end to end: a real bound
/// `HealthListener` sitting with no clients, a real idle timeout firing
/// against the `DaemonState` it shares, the wake that has to reach the accept
/// loop, and the `join()` that `run_daemon`'s step 7 performs on the listener
/// thread actually returning.
///
/// Coverage was piecewise before this — `daemon_idle_timeout_shuts_down_daemon`
/// mocks the arithmetic with no listener, and the health unit tests drive
/// `request_shutdown` directly with no idle path — so nothing failed if the two
/// halves stopped composing. Red if the wake is lost *and* the accept loop's
/// readiness floor is removed: `run` then never returns and the join hangs
/// forever, which is what the daemon would do in production.
///
/// Needs no state-directory isolation beyond its own tempdir: the listener
/// binds a socket there, and nothing here writes a discovery record or
/// otherwise touches `$HOME`.
#[test]
fn idle_timeout_stops_a_real_health_listener() {
    const IDLE_TIMEOUT: Duration = Duration::from_millis(500);
    const MONITOR_TICK: Duration = Duration::from_millis(25);
    // Generous against the ~500 ms the idle path needs, and still far short of
    // the unbounded wait a broken accept loop would produce.
    const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(15);

    let (_dir, endpoint) = test_endpoint();
    let listener = HealthListener::bind(&endpoint).expect("bind real health listener");
    let state = Arc::new(DaemonState::new(endpoint.clone()));
    let info = Arc::new(Mutex::new(test_daemon_info(&endpoint)));

    let run_state = Arc::clone(&state);
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let health_thread = std::thread::spawn(move || {
        listener.run(run_state, info);
        let _ = finished_tx.send(());
    });

    // PING rather than HEARTBEAT: it proves the listener is really serving
    // before the clock matters, without resetting the idle timer the test is
    // about.
    let pong =
        health::send_command(&endpoint, "PING").expect("real health listener must answer PING");
    assert!(pong.starts_with("PONG"), "unexpected PING reply: {pong:?}");
    // The same decision `run.rs`'s `idle_monitor` makes, at a tick that suits a
    // test: no activity for `IDLE_TIMEOUT` ⇒ request shutdown.
    let monitor_state = Arc::clone(&state);
    let monitor = std::thread::spawn(move || {
        loop {
            std::thread::sleep(MONITOR_TICK);
            if monitor_state.should_shutdown() {
                return;
            }
            if monitor_state.idle_duration() >= IDLE_TIMEOUT {
                monitor_state.request_shutdown();
                return;
            }
        }
    });

    let started_at = Instant::now();
    let stopped = finished_rx.recv_timeout(SHUTDOWN_DEADLINE);
    let elapsed = started_at.elapsed();

    monitor.join().expect("idle monitor must not panic");
    assert!(
        stopped.is_ok(),
        "the health listener was still running {elapsed:?} after an idle timeout fired; \
         run_daemon joins this thread, so an accept loop that cannot be stopped is a \
         permanent hang rather than a slow shutdown"
    );
    health_thread
        .join()
        .expect("health listener thread must not panic");

    assert!(state.should_shutdown());
    assert!(
        elapsed >= IDLE_TIMEOUT,
        "the listener stopped after {elapsed:?}, before the {IDLE_TIMEOUT:?} idle period \
         elapsed — something other than the idle path shut it down"
    );
}

#[test]
fn daemon_heartbeat_prevents_idle_shutdown() {
    let (_dir, state) = new_daemon_state();
    let state = Arc::new(state);
    let idle_timeout = Duration::from_secs(1);

    let monitor_state = Arc::clone(&state);
    let heartbeat_state = Arc::clone(&state);

    // Reports back the instant taken just before its final `touch()`. The
    // monitor counts from what that `touch()` stored, so measuring from this
    // reference — rather than from whenever the heartbeat thread happened to
    // be joined — keeps the wait below tied to the reset the monitor saw.
    let heartbeat = std::thread::spawn(move || {
        let start = Instant::now();
        let mut last_heartbeat = start;
        while start.elapsed() < Duration::from_millis(1500) {
            last_heartbeat = Instant::now();
            heartbeat_state.touch();
            std::thread::sleep(Duration::from_millis(200));
        }
        last_heartbeat
    });

    let monitor = std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if monitor_state.idle_duration() >= idle_timeout {
                monitor_state.request_shutdown();
                break;
            }
            if monitor_state.should_shutdown() {
                break;
            }
        }
    });

    let last_heartbeat = heartbeat.join().unwrap();
    monitor.join().unwrap();
    let waited = last_heartbeat.elapsed();

    assert!(state.should_shutdown());
    assert!(
        waited >= idle_timeout,
        "daemon shut down {waited:?} after the last heartbeat, before the \
         {idle_timeout:?} idle period elapsed"
    );
}

// ─── Unit tests: Discovery file (require ENV_LOCK) ────────────────────────────

#[test]
fn daemon_info_literal_serializes_the_health_endpoint_field() {
    let info = DaemonInfo {
        pid: 12345,
        hyperd_endpoint: "127.0.0.1:54321".to_string(),
        health_endpoint: "/tmp/state/daemon.sock".to_string(),
        started_at: "2026-05-20T10:30:00Z".to_string(),
        version: "0.1.3".to_string(),
    };

    assert_eq!(
        serde_json::to_value(info).unwrap(),
        serde_json::json!({
            "pid": 12345,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_endpoint": "/tmp/state/daemon.sock",
            "started_at": "2026-05-20T10:30:00Z",
            "version": "0.1.3"
        })
    );
}

#[test]
fn discovery_file_write_and_read() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let info = DaemonInfo {
        pid: 12345,
        hyperd_endpoint: "127.0.0.1:54321".to_string(),
        health_endpoint: "/tmp/state/daemon.sock".to_string(),
        started_at: "2026-05-20T10:30:00Z".to_string(),
        version: "0.1.3".to_string(),
    };

    discovery::write_discovery_file(&info).unwrap();

    let path = tmp.path().join("daemon.json");
    assert!(path.exists());

    let contents = std::fs::read_to_string(&path).unwrap();
    let read_back: DaemonInfo = serde_json::from_str(&contents).unwrap();
    assert_eq!(read_back.pid, 12345);
    assert_eq!(read_back.hyperd_endpoint, "127.0.0.1:54321");
    assert_eq!(read_back.health_endpoint, "/tmp/state/daemon.sock");
    assert_eq!(read_back.version, "0.1.3");
}

#[test]
fn discovery_file_overwrite_replaces_content() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let info1 = DaemonInfo {
        pid: 100,
        hyperd_endpoint: "127.0.0.1:1111".to_string(),
        health_endpoint: "/tmp/state/daemon.sock".to_string(),
        started_at: "2026-01-01T00:00:00Z".to_string(),
        version: "0.1.0".to_string(),
    };
    discovery::write_discovery_file(&info1).unwrap();

    let info2 = DaemonInfo {
        pid: 200,
        hyperd_endpoint: "127.0.0.1:2222".to_string(),
        health_endpoint: "/tmp/other/daemon.sock".to_string(),
        started_at: "2026-02-02T00:00:00Z".to_string(),
        version: "0.2.0".to_string(),
    };
    discovery::write_discovery_file(&info2).unwrap();

    let path = tmp.path().join("daemon.json");
    let contents = std::fs::read_to_string(&path).unwrap();
    let read_back: DaemonInfo = serde_json::from_str(&contents).unwrap();
    assert_eq!(read_back.pid, 200);
    assert_eq!(read_back.hyperd_endpoint, "127.0.0.1:2222");
    assert_eq!(read_back.health_endpoint, "/tmp/other/daemon.sock");
}

#[test]
fn remove_discovery_file_deletes_it() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let info = DaemonInfo {
        pid: 1,
        hyperd_endpoint: "127.0.0.1:1".to_string(),
        health_endpoint: "/tmp/state/daemon.sock".to_string(),
        started_at: "2026-01-01T00:00:00Z".to_string(),
        version: "0.0.1".to_string(),
    };
    discovery::write_discovery_file(&info).unwrap();
    let path = tmp.path().join("daemon.json");
    assert!(path.exists());

    discovery::remove_discovery_file();
    assert!(!path.exists());
}

#[test]
fn discover_returns_none_when_no_file_exists() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    assert!(discovery::discover().is_none());
}

/// A record whose endpoint nobody serves is not a daemon. Unlike the previous
/// `discover`, it is also *not deleted* here: removing a stale record is the
/// job of whoever holds the daemon lock, because only a held lock proves no
/// daemon is starting.
#[test]
fn discover_returns_none_for_stale_record_and_leaves_it() {
    let tmp = TempDir::new().unwrap();
    let endpoint = HealthEndpoint::for_new_daemon(tmp.path()).unwrap();
    let record = write_record(tmp.path(), &test_daemon_info(&endpoint));

    assert!(discovery::discover_in(tmp.path()).is_none());

    assert_eq!(
        std::fs::read(tmp.path().join("daemon.json")).unwrap(),
        record,
        "discovery must be side-effect free"
    );
}

#[test]
fn discover_finds_live_daemon() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let health = start_health_listener_in(tmp.path());

    let info = test_daemon_info(&health.endpoint);
    discovery::write_discovery_file(&info).unwrap();

    let discovered = discovery::discover().expect("should discover live daemon");
    assert_eq!(discovered.pid, 12345);
    assert_eq!(discovered.health_endpoint, health.endpoint.as_str());
}

/// A record naming a live socket that is not this state directory's own
/// `daemon.sock` must never be connected to: the record is attacker-writable
/// input as far as the client is concerned. Unix only, where the endpoint is a
/// path derived from the state directory (a Windows pipe name is random).
#[cfg(unix)]
#[test]
fn discover_never_connects_to_an_endpoint_outside_the_state_dir() {
    let state = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let mut foreign = FakeHealthPeer::start_in(elsewhere.path(), |command| match command {
        "PING" => pong_line().into_bytes(),
        _ => b"ERR\n".to_vec(),
    });
    write_record(state.path(), &test_daemon_info(&foreign.endpoint));

    assert!(
        discovery::discover_in(state.path()).is_none(),
        "a record pointing at a live foreign socket must not be believed"
    );
    assert!(
        foreign.stop_and_join().is_empty(),
        "the client must not even connect to a socket outside its state directory"
    );
}

/// A state directory another user could write to may hold a record steering
/// the client anywhere, so discovery refuses it outright, live daemon or not.
/// Group-writable is testable without root; "owned by someone else" takes
/// the same branch of `verify_state_dir_trusted` but needs a second user.
#[cfg(unix)]
#[test]
fn discover_refuses_a_group_writable_state_dir_even_with_a_live_daemon() {
    let tmp = TempDir::new().unwrap();
    let mut peer = FakeHealthPeer::start_in(tmp.path(), |_| pong_line().into_bytes());
    write_record(tmp.path(), &test_daemon_info(&peer.endpoint));
    assert!(
        discovery::discover_in(tmp.path()).is_some(),
        "control: the same directory is discoverable while private"
    );
    peer.stop_and_join();

    let mut peer = FakeHealthPeer::start_in(tmp.path(), |_| pong_line().into_bytes());
    make_group_writable(tmp.path());

    assert!(discovery::discover_in(tmp.path()).is_none());
    assert!(
        peer.stop_and_join().is_empty(),
        "an untrusted state directory must be rejected before any connection"
    );
}

/// `ensure_daemon` on an untrusted state directory fails and spawns nothing
/// (and `Engine` then falls back to a private `hyperd`). Observable without a
/// spawner seam: a spawn would create `daemon.json` / `daemon.lock` /
/// `daemon.sock` here, or at least leave a process behind.
#[cfg(unix)]
#[test]
fn ensure_daemon_refuses_an_untrusted_state_dir_without_spawning() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let mut peer = FakeHealthPeer::start_in(tmp.path(), |_| pong_line().into_bytes());
    write_record(tmp.path(), &test_daemon_info(&peer.endpoint));
    make_group_writable(tmp.path());
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let started = Instant::now();
    let error = hyperdb_mcp::daemon::spawn::ensure_daemon()
        .expect_err("an untrusted state directory must not yield a daemon");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "refusal must be immediate, not a spawn wait"
    );
    assert!(
        !tmp.path().join(control_lock_name()).exists(),
        "no daemon lock was ever taken, so nothing was spawned"
    );
    assert!(
        peer.stop_and_join().is_empty(),
        "the untrusted directory's socket must not be contacted"
    );
}

/// The user-visible end of the same rule: with an untrusted state directory
/// the engine does not use the (apparently live) daemon but runs its own
/// private `hyperd`.
#[cfg(unix)]
#[test]
fn engine_falls_back_to_local_mode_in_an_untrusted_state_dir() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let mut peer = FakeHealthPeer::start_in(tmp.path(), |_| pong_line().into_bytes());
    write_record(tmp.path(), &test_daemon_info(&peer.endpoint));
    make_group_writable(tmp.path());
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let engine = hyperdb_mcp::engine::Engine::new(None)
        .expect("an untrusted state directory must fall back to a private hyperd");

    assert!(
        engine.daemon_health_endpoint().is_none(),
        "the engine must be in local mode, not attached to the daemon"
    );
    drop(engine);
    assert!(
        peer.stop_and_join().is_empty(),
        "the untrusted directory's socket must not be contacted"
    );
}

// ─── Unit tests: Version takeover decision (no env vars, safe parallel) ─────────

#[test]
fn takeover_decision_newer_client_takes_over() {
    assert!(
        hyperdb_mcp::daemon::spawn::client_should_take_over("0.5.0", "0.4.0"),
        "0.5.0 client should take over 0.4.0 daemon"
    );
}

#[test]
fn takeover_decision_equal_version_reuses() {
    assert!(
        !hyperdb_mcp::daemon::spawn::client_should_take_over("0.4.0", "0.4.0"),
        "equal versions should reuse daemon"
    );
}

#[test]
fn takeover_decision_older_client_reuses() {
    assert!(
        !hyperdb_mcp::daemon::spawn::client_should_take_over("0.4.0", "0.5.0"),
        "older client should reuse newer daemon"
    );
}

#[test]
fn takeover_decision_client_unparseable_reuses() {
    assert!(
        !hyperdb_mcp::daemon::spawn::client_should_take_over("garbage", "0.4.0"),
        "unparseable client version should reuse daemon"
    );
}

#[test]
fn takeover_decision_daemon_unparseable_reuses() {
    assert!(
        !hyperdb_mcp::daemon::spawn::client_should_take_over("0.4.0", "garbage"),
        "unparseable daemon version should reuse daemon"
    );
}

#[test]
fn takeover_decision_both_unparseable_reuses() {
    assert!(
        !hyperdb_mcp::daemon::spawn::client_should_take_over("garbage", "junk"),
        "both unparseable should reuse daemon"
    );
}

// ─── CLI tests: `daemon status` / `daemon stop` / `daemon` against fakes ──────

/// With no daemon at all, `status` and `stop` say so and exit 1.
#[test]
fn daemon_cli_status_and_stop_without_a_daemon_exit_1() {
    let state = TempDir::new().unwrap();

    for action in ["status", "stop"] {
        let output = run_cli_bounded(&["daemon", action], state.path(), Duration::from_secs(5));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "daemon {action}: {stderr}");
        assert!(
            stderr.contains("No daemon is currently running."),
            "daemon {action} stderr was {stderr:?}"
        );
    }
}

/// A record left behind by a dead daemon (right shape, nobody serving the
/// socket) is not a daemon: the CLI reports none, and does not clean up —
/// cleanup belongs to whoever holds the daemon lock.
#[test]
fn daemon_cli_status_ignores_a_stale_record_and_leaves_it() {
    let state = TempDir::new().unwrap();
    let endpoint = HealthEndpoint::for_new_daemon(state.path()).unwrap();
    let record = write_record(state.path(), &test_daemon_info(&endpoint));

    let output = run_cli_bounded(&["daemon", "status"], state.path(), Duration::from_secs(5));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("No daemon is currently running."),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read(state.path().join("daemon.json")).unwrap(),
        record,
        "`daemon status` must not delete or rewrite the record"
    );
}

/// A pre-1.0 daemon holds no lock and speaks TCP, so the 1.0 CLI cannot stop
/// it; `daemon stop` and `daemon status` say that it is recorded, with its pid
/// and port, instead of a bare "no daemon".
#[test]
fn daemon_cli_names_a_recorded_pre_1_0_daemon() {
    let state = TempDir::new().unwrap();
    std::fs::write(
        state.path().join("daemon.json"),
        serde_json::json!({
            "pid": 424_242,
            "hyperd_endpoint": "127.0.0.1:54321",
            "health_port": 7485,
            "started_at": "2026-08-13T12:34:56Z",
            "version": "0.7.0",
        })
        .to_string(),
    )
    .unwrap();
    for action in ["stop", "status"] {
        let output = run_cli_bounded(&["daemon", action], state.path(), Duration::from_secs(5));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{action}: {stderr}");
        assert!(
            stderr.contains("pre-1.0 daemon")
                && stderr.contains("424242")
                && stderr.contains("7485"),
            "{action}: {stderr}"
        );
    }
    assert!(
        !state.path().join("daemon.lock").exists(),
        "the CLI must not create daemon.lock"
    );
}

/// `daemon status` talks to the endpoint recorded in the state directory
/// (selected by `HYPERDB_STATE_DIR`) and prints that daemon's own STATUS.
#[test]
fn daemon_status_cli_queries_the_recorded_endpoint() {
    let state = TempDir::new().unwrap();
    let endpoint = HealthEndpoint::for_new_daemon(state.path()).unwrap();
    let info = DaemonInfo {
        pid: 515_151,
        ..test_daemon_info(&endpoint)
    };
    write_record(state.path(), &info);
    let mut peer = FakeHealthPeer::start_at(&endpoint, scripted_daemon(&info));

    let output = run_cli_bounded(&["daemon", "status"], state.path(), Duration::from_secs(10));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "`daemon status` failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains(&format!("Health endpoint: {}", endpoint.as_str())),
        "status must report the endpoint it asked\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("515151"),
        "status must report the pid\n{stdout}"
    );

    let commands = peer.stop_and_join();
    assert_eq!(
        commands
            .iter()
            .filter(|command| *command == "STATUS")
            .count(),
        1,
        "exactly one STATUS expected, got {commands:?}"
    );
}

/// `daemon stop` sends STOP to the recorded endpoint and relays the answer.
#[test]
fn daemon_stop_cli_sends_stop_to_the_recorded_endpoint() {
    let state = TempDir::new().unwrap();
    let endpoint = HealthEndpoint::for_new_daemon(state.path()).unwrap();
    let info = test_daemon_info(&endpoint);
    write_record(state.path(), &info);
    let mut peer = FakeHealthPeer::start_at(&endpoint, scripted_daemon(&info));

    let output = run_cli_bounded(&["daemon", "stop"], state.path(), Duration::from_secs(10));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "`daemon stop` failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("Daemon responded: STOPPING"),
        "stdout was {stdout:?}"
    );
    assert!(
        peer.stop_and_join().iter().any(|command| command == "STOP"),
        "the peer never received STOP"
    );
}

/// A live daemon whose `daemon.json` is gone is still reachable by `daemon
/// stop` and `daemon status`: the held lock plus an identified `PING` on the
/// well-known socket find it. Unix only (a Windows pipe name lives in the record).
#[cfg(unix)]
#[test]
fn daemon_cli_reaches_a_live_daemon_whose_record_is_missing() {
    let state = TempDir::new().unwrap();
    let endpoint = HealthEndpoint::for_new_daemon(state.path()).unwrap();
    let info = test_daemon_info(&endpoint);
    // No record is written. The held lock stands in for the live daemon.
    let lock = DaemonLock::try_acquire(state.path())
        .unwrap()
        .expect("fresh state directory has a free lock");
    let stop_seen = Arc::new(AtomicBool::new(false));
    let mut peer = FakeHealthPeer::start_at(
        &endpoint,
        scripted_daemon_noting_stop(&info, Arc::clone(&stop_seen)),
    );

    let status = run_cli_bounded(&["daemon", "status"], state.path(), Duration::from_secs(10));
    assert!(
        status.status.success(),
        "`daemon status` failed: {}\nstderr:\n{}",
        status.status,
        String::from_utf8_lossy(&status.stderr)
    );
    // `daemon stop` succeeds only once the lock is free, so let go of it a
    // moment after the peer has actually received STOP (not on a fixed delay
    // that a loaded machine could outrun).
    let releaser = std::thread::spawn(move || {
        wait_for_flag(&stop_seen, Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(300));
        drop(lock);
    });
    let stop = run_cli_bounded(&["daemon", "stop"], state.path(), Duration::from_secs(10));
    releaser.join().unwrap();
    assert!(
        stop.status.success(),
        "`daemon stop` failed: {}\nstderr:\n{}",
        stop.status,
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(
        peer.stop_and_join().iter().any(|command| command == "STOP"),
        "the peer never received STOP"
    );
}

/// `daemon stop` returns only once the daemon has let go of its lock, not when
/// it merely acknowledges `STOP`. The fake daemon acknowledges at once but
/// releases the lock 1.5 s later; the CLI must not have returned before that.
#[cfg(unix)]
#[test]
fn daemon_stop_cli_returns_only_after_the_lock_is_free() {
    let state = TempDir::new().unwrap();
    let endpoint = HealthEndpoint::for_new_daemon(state.path()).unwrap();
    let info = test_daemon_info(&endpoint);
    write_record(state.path(), &info);
    let lock = DaemonLock::try_acquire(state.path())
        .unwrap()
        .expect("fresh state directory has a free lock");
    let stop_seen = Arc::new(AtomicBool::new(false));
    let mut peer = FakeHealthPeer::start_at(
        &endpoint,
        scripted_daemon_noting_stop(&info, Arc::clone(&stop_seen)),
    );

    let lock_released = Arc::new(AtomicBool::new(false));
    let released_by_thread = Arc::clone(&lock_released);
    let freer = std::thread::spawn(move || {
        // Hold the lock for 1.5 s after STOP arrives: the CLI must outwait it.
        wait_for_flag(&stop_seen, Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(1500));
        released_by_thread.store(true, Ordering::SeqCst);
        drop(lock);
    });
    let output = run_cli_bounded(&["daemon", "stop"], state.path(), Duration::from_secs(20));
    let released_when_cli_returned = lock_released.load(Ordering::SeqCst);
    freer.join().unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "`daemon stop` failed: {stderr}");
    assert!(
        released_when_cli_returned,
        "`daemon stop` returned while the daemon still held the lock"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Daemon stopped."),
        "stdout was {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(peer.stop_and_join().iter().any(|command| command == "STOP"));
}

/// A held lock with no record and nothing answering gets a clear message, not
/// "No daemon is currently running."
#[test]
fn daemon_cli_explains_a_held_lock_with_no_usable_record() {
    let state = TempDir::new().unwrap();
    let _lock = DaemonLock::try_acquire(state.path())
        .unwrap()
        .expect("fresh state directory has a free lock");

    for args in [["daemon", "status"], ["daemon", "stop"]] {
        let output = run_cli_bounded(&args, state.path(), Duration::from_secs(10));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(
            stderr.contains("daemon lock is held but no usable record was found"),
            "{args:?} stderr was {stderr:?}"
        );
        assert!(
            !stderr.contains("No daemon is currently running"),
            "{stderr}"
        );
    }
}

/// A record naming a live socket outside the state directory is rejected by the
/// CLI without connecting to it. Unix only (see
/// `discover_never_connects_to_an_endpoint_outside_the_state_dir`).
#[cfg(unix)]
#[test]
fn daemon_cli_does_not_follow_a_record_to_a_foreign_socket() {
    let state = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let foreign_info = test_daemon_info(&HealthEndpoint::for_new_daemon(elsewhere.path()).unwrap());
    let mut foreign = FakeHealthPeer::start_in(elsewhere.path(), scripted_daemon(&foreign_info));
    write_record(state.path(), &foreign_info);

    let output = run_cli_bounded(&["daemon", "status"], state.path(), Duration::from_secs(5));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("No daemon is currently running."),
        "{stderr}"
    );
    assert!(
        foreign.stop_and_join().is_empty(),
        "the CLI connected to a socket that is not its state directory's"
    );
}

/// A group-writable state directory is untrusted: the CLI reports no daemon and
/// never contacts the socket in it, even though a live daemon is answering.
#[cfg(unix)]
#[test]
fn daemon_cli_does_not_trust_a_group_writable_state_dir() {
    let state = TempDir::new().unwrap();
    let endpoint = HealthEndpoint::for_new_daemon(state.path()).unwrap();
    let info = test_daemon_info(&endpoint);
    write_record(state.path(), &info);
    let mut peer = FakeHealthPeer::start_at(&endpoint, scripted_daemon(&info));
    make_group_writable(state.path());

    let output = run_cli_bounded(&["daemon", "status"], state.path(), Duration::from_secs(5));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("cannot be trusted"), "{stderr}");
    assert!(
        peer.stop_and_join().is_empty(),
        "an untrusted state directory's socket must not be contacted"
    );
}

// ─── Single-instance tests: the daemon lock, not a port, decides ──────────────

/// While a daemon holds the lock and answers PING, a second `hyperdb-mcp
/// daemon` in the same state directory is refused promptly with the
/// "already running" message.
///
/// Replaces `health_listener_second_bind_same_port_fails`. What this proves:
/// the second start sees a held lock and a live, PING-answering daemon for the
/// debounce interval, and bails *before* binding anything or spawning a
/// `hyperd` — the pre-existing record and the live socket are untouched (a
/// second daemon would have rewritten the record), and the live peer only ever
/// saw PINGs. It does not start a real `hyperd`; the real-process variant is
/// `second_daemon_in_the_same_state_dir_is_refused_while_the_first_runs`.
#[test]
fn second_daemon_is_refused_while_the_lock_is_held_by_a_live_daemon() {
    let state = TempDir::new().unwrap();
    let endpoint = HealthEndpoint::for_new_daemon(state.path()).unwrap();
    let info = test_daemon_info(&endpoint);
    let held = DaemonLock::try_acquire(state.path())
        .unwrap()
        .expect("fresh state directory is lockable");
    let record = write_record(state.path(), &info);
    let mut peer = FakeHealthPeer::start_at(&endpoint, scripted_daemon(&info));

    let started = Instant::now();
    let output = run_cli_bounded(&["daemon"], state.path(), Duration::from_secs(20));
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "a second daemon must not start; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("already running"),
        "stderr must say a daemon is already running, got:\n{stderr}"
    );
    assert!(
        elapsed < hyperdb_mcp::daemon::run::STARTUP_WAIT,
        "the refusal took {elapsed:?}; a live daemon must be detected after \
         about a second, not after the {:?} startup wait",
        hyperdb_mcp::daemon::run::STARTUP_WAIT
    );
    assert_eq!(
        std::fs::read(state.path().join("daemon.json")).unwrap(),
        record,
        "the refused daemon must not touch the running daemon's record"
    );
    let commands = peer.stop_and_join();
    assert!(
        !commands.is_empty() && commands.iter().all(|command| command == "PING"),
        "the live daemon should only have been PINGed, got {commands:?}"
    );
    drop(held);
}

/// A held lock with nothing answering means a daemon that is starting or
/// stopping: a new daemon waits the full startup budget for the lock and then
/// gives up with a lock-specific message instead of racing it.
#[test]
fn second_daemon_waits_out_a_lock_held_without_a_daemon_then_times_out() {
    let state = TempDir::new().unwrap();
    let _held = DaemonLock::try_acquire(state.path())
        .unwrap()
        .expect("fresh state directory is lockable");

    let started = Instant::now();
    let output = run_cli_bounded(&["daemon"], state.path(), Duration::from_secs(30));
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "stderr:\n{stderr}");
    assert!(
        stderr.contains("daemon lock"),
        "stderr must name the lock, got:\n{stderr}"
    );
    assert!(
        elapsed + Duration::from_secs(1) >= hyperdb_mcp::daemon::run::STARTUP_WAIT,
        "gave up after {elapsed:?}; a starting/stopping daemon deserves the full wait"
    );
}

// ─── Integration tests: full daemon lifecycle with real hyperd ─────────────────

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn daemon_mode_engine_connects_to_shared_hyperd() {
    let _lock = acquire_env_lock();
    let daemon = TestDaemon::start();

    let tmp = TempDir::new().unwrap();
    let workspace_path = tmp.path().join("test.hyper");

    let engine =
        hyperdb_mcp::engine::Engine::new(Some(workspace_path.to_str().unwrap().to_string()))
            .expect("engine should connect to daemon");

    assert!(engine.is_running());

    let endpoint = engine.hyperd_endpoint().unwrap();
    assert_eq!(endpoint, daemon.info.hyperd_endpoint);
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn daemon_mode_two_engines_share_same_hyperd() {
    let _lock = acquire_env_lock();
    let daemon = TestDaemon::start();

    let tmp1 = TempDir::new().unwrap();
    let tmp2 = TempDir::new().unwrap();
    let path1 = tmp1.path().join("db1.hyper");
    let path2 = tmp2.path().join("db2.hyper");

    let engine1 =
        hyperdb_mcp::engine::Engine::new(Some(path1.to_str().unwrap().to_string())).unwrap();

    let engine2 =
        hyperdb_mcp::engine::Engine::new(Some(path2.to_str().unwrap().to_string())).unwrap();

    // Both engines should be in daemon mode (connected to the same daemon).
    // We verify via the health endpoint rather than the hyperd endpoint, because
    // the daemon's liveness monitor can restart hyperd (changing the endpoint)
    // between the two Engine::new calls.
    let ep1 = engine1.hyperd_endpoint().unwrap();
    let ep2 = engine2.hyperd_endpoint().unwrap();
    assert!(
        !ep1.is_empty() && !ep2.is_empty(),
        "both engines must report a daemon endpoint"
    );
    // Verify the daemon is the one we started (health endpoint reachable)
    let status = health::send_command(&daemon.endpoint, "PING").unwrap();
    assert!(status.trim().starts_with("PONG hyperdb-mcp "));

    engine1.execute_command("CREATE TABLE foo (x INT)").unwrap();
    engine1
        .execute_command("INSERT INTO foo VALUES (42)")
        .unwrap();

    let tables = engine2.describe_tables().unwrap();
    assert!(
        tables.iter().all(|t| t["name"] != "foo"),
        "engine2 should not see engine1's table"
    );
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn daemon_mode_persistent_database_file_survives_engine_drop() {
    let _lock = acquire_env_lock();
    let _daemon = TestDaemon::start();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("persistent.hyper");
    let path_str = path.to_str().unwrap().to_string();

    {
        let engine = hyperdb_mcp::engine::Engine::new(Some(path_str.clone())).unwrap();
        engine
            .execute_command("CREATE TABLE survive (val TEXT)")
            .unwrap();
        engine
            .execute_command("INSERT INTO survive VALUES ('hello')")
            .unwrap();
    }

    assert!(
        path.exists(),
        "persistent .hyper file should survive engine drop"
    );
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn daemon_mode_persistent_engine_data_is_queryable() {
    let _lock = acquire_env_lock();
    let daemon = TestDaemon::start();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("queryable.hyper");
    let path_str = path.to_str().unwrap().to_string();

    let engine = hyperdb_mcp::engine::Engine::new(Some(path_str)).unwrap();
    engine
        .execute_command("CREATE TABLE items (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_command("INSERT INTO items VALUES (1, 'alpha'), (2, 'beta')")
        .unwrap();

    let rows = engine
        .execute_query_to_json("SELECT * FROM items ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "alpha");
    assert_eq!(rows[1]["name"], "beta");

    let resp = health::send_command(&daemon.endpoint, "PING").unwrap();
    assert!(resp.trim().starts_with("PONG hyperdb-mcp "));
}

#[cfg(unix)]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn hyperd_monitor_detects_killed_hyperd_and_restarts() {
    let _lock = acquire_env_lock();
    let daemon = TestDaemon::start();

    let pid_before = find_hyperd_pid_for_endpoint(&daemon.info.hyperd_endpoint)
        .expect("should find hyperd pid for endpoint before kill");

    // SIGKILL the hyperd process. The daemon's monitor should detect this on
    // the next 5s tick and restart hyperd.
    kill_pid(pid_before);

    // Wait for the monitor to fire and restart hyperd. The budget is generous
    // (5s monitor tick + cold spawn + slack) so a loaded CI runner doesn't trip
    // a false timeout; see RESTART_READINESS_BUDGET_SECS.
    let new_endpoint = wait_for_live_hyperd_after_kill(
        &daemon.endpoint,
        &daemon.info.hyperd_endpoint,
        RESTART_READINESS_BUDGET_SECS,
    )
    .unwrap_or_else(|| {
        panic!("daemon should restart hyperd within {RESTART_READINESS_BUDGET_SECS}s")
    });

    // The new endpoint must be reachable. Don't assert it differs from the old —
    // the OS may reuse a TCP port, and the Unix socket path is fixed by design.
    assert!(
        endpoint_accepts_connection(&new_endpoint),
        "new hyperd endpoint should be reachable"
    );
}

#[cfg(unix)]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn client_report_triggers_restart_after_kill() {
    let _lock = acquire_env_lock();
    let daemon = TestDaemon::start();

    let pid_before = find_hyperd_pid_for_endpoint(&daemon.info.hyperd_endpoint)
        .expect("should find hyperd pid before kill");
    kill_pid(pid_before);

    // Immediately tell the daemon hyperd is dead — don't wait for the monitor.
    // Even so, the monitor only reacts on its 5s tick, so the worst-case
    // recovery time is unchanged. This test just verifies the report path
    // triggers the same restart as detection-via-polling.
    let response = health::send_command(&daemon.endpoint, "REPORT_HYPERD_ERROR").unwrap();
    assert_eq!(response.trim(), "OK");

    let new_endpoint = wait_for_live_hyperd_after_kill(
        &daemon.endpoint,
        &daemon.info.hyperd_endpoint,
        RESTART_READINESS_BUDGET_SECS,
    )
    .unwrap_or_else(|| {
        panic!("daemon should restart hyperd within {RESTART_READINESS_BUDGET_SECS}s after report")
    });

    assert!(
        endpoint_accepts_connection(&new_endpoint),
        "new hyperd endpoint should be reachable"
    );
}

#[cfg(unix)]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn engine_recovers_after_hyperd_killed() {
    // End-to-end test: the user-visible behavior of this whole feature.
    // 1. Start daemon + create an Engine (= an MCP client connection).
    // 2. Run a query — it succeeds.
    // 3. SIGKILL hyperd.
    // 4. Wait for the daemon to restart it.
    // 5. Run another query through the same recovery path the server uses
    //    (drop engine on ConnectionLost, then create a fresh engine).
    let _lock = acquire_env_lock();
    let daemon = TestDaemon::start();

    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("recover.hyper");
    let path_str = path.to_str().unwrap().to_string();

    // Engine #1: pre-kill. Write data into the persistent attachment via
    // fully-qualified SQL so it survives the engine drop and the
    // hyperd restart (the engine's ephemeral primary would not).
    {
        let engine = hyperdb_mcp::engine::Engine::new(Some(path_str.clone())).unwrap();
        engine
            .execute_command("CREATE TABLE \"persistent\".\"public\".\"keepers\" (n INT)")
            .unwrap();
        engine
            .execute_command(
                "INSERT INTO \"persistent\".\"public\".\"keepers\" VALUES (1), (2), (3)",
            )
            .unwrap();
    }

    // Find and kill hyperd
    let pid =
        find_hyperd_pid_for_endpoint(&daemon.info.hyperd_endpoint).expect("should find hyperd pid");
    kill_pid(pid);

    // Wait for daemon-side restart. The daemon persists `daemon.json` before
    // it flips what STATUS serves, so a live endpoint from STATUS means the
    // discovery file `Engine::new` reads below is already committed.
    wait_for_live_hyperd_after_kill(
        &daemon.endpoint,
        &daemon.info.hyperd_endpoint,
        RESTART_READINESS_BUDGET_SECS,
    )
    .unwrap_or_else(|| {
        panic!("daemon should restart hyperd within {RESTART_READINESS_BUDGET_SECS}s")
    });

    // Engine #2: post-restart. This mirrors what `with_engine` does after a
    // ConnectionLost — drop the old engine (already done above) and create a
    // fresh one. The fresh engine re-discovers the daemon and connects to the
    // new endpoint, then re-attaches the persistent file.
    let engine = hyperdb_mcp::engine::Engine::new(Some(path_str)).unwrap();
    let rows = engine
        .execute_query_to_json("SELECT n FROM \"persistent\".\"public\".\"keepers\" ORDER BY n")
        .unwrap();
    assert_eq!(rows.len(), 3, "data persisted across hyperd restart");
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn daemon_mode_ephemeral_database_cleaned_up_on_drop() {
    let _lock = acquire_env_lock();
    let _daemon = TestDaemon::start();

    let engine = hyperdb_mcp::engine::Engine::new(None).unwrap();
    let ephemeral_path = engine.ephemeral_path().to_path_buf();

    assert!(ephemeral_path.exists());

    engine
        .execute_command("CREATE TABLE ephemeral_test (id INT)")
        .unwrap();

    drop(engine);

    assert!(
        !ephemeral_path.exists(),
        "ephemeral .hyper file should be deleted after engine drop"
    );
}

/// The real-process variant of the single-instance guarantee: with a genuine
/// daemon (and its `hyperd`) running, a second `hyperdb-mcp daemon` process in
/// the same state directory is refused with "already running", and the first
/// daemon is left undisturbed — same record, same pid, still answering.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn second_daemon_in_the_same_state_dir_is_refused_while_the_first_runs() {
    let _lock = acquire_env_lock();
    let daemon = TestDaemon::start();
    let record = std::fs::read(daemon.state_dir.join("daemon.json")).unwrap();

    let started = Instant::now();
    let output = run_cli_bounded(&["daemon"], &daemon.state_dir, Duration::from_secs(30));
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "a second daemon must not start; stderr:\n{stderr}"
    );
    assert!(stderr.contains("already running"), "stderr:\n{stderr}");
    assert!(
        elapsed < hyperdb_mcp::daemon::run::STARTUP_WAIT,
        "refusal took {elapsed:?}"
    );

    assert_eq!(
        std::fs::read(daemon.state_dir.join("daemon.json")).unwrap(),
        record,
        "the refused daemon must not have rewritten the record"
    );
    let status = health::send_command(&daemon.endpoint, "STATUS").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(status.trim()).unwrap();
    assert_eq!(
        parsed["pid"],
        std::process::id(),
        "the original (in-process) daemon must still be the one answering"
    );
    assert_eq!(parsed["hyperd_endpoint"], daemon.info.hyperd_endpoint);
}

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// A per-test state directory and the health endpoint inside it. The directory
/// is short (a system temp dir plus a random name) so that
/// `<dir>/daemon.sock` stays under the platform's `sun_path` limit.
fn test_endpoint() -> (TempDir, HealthEndpoint) {
    let dir = TempDir::new().expect("create short per-test state dir");
    let endpoint = HealthEndpoint::for_new_daemon(dir.path()).expect("derive health endpoint");
    (dir, endpoint)
}

/// The reply a genuine daemon gives to `PING`.
fn pong_line() -> String {
    format!("PONG hyperdb-mcp {}\n", hyperdb_mcp::version::MCP_VERSION)
}

/// A `DaemonInfo` for a daemon serving `endpoint`, with a fixed identity.
fn test_daemon_info(endpoint: &HealthEndpoint) -> DaemonInfo {
    DaemonInfo {
        pid: 12345,
        hyperd_endpoint: "127.0.0.1:54321".to_string(),
        health_endpoint: endpoint.as_str().to_string(),
        started_at: "2026-05-20T10:30:00Z".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Write `info` as `daemon.json` in `state_dir` and return the exact bytes, so
/// a test can later prove the record was left alone.
fn write_record(state_dir: &Path, info: &DaemonInfo) -> Vec<u8> {
    let bytes = serde_json::to_vec_pretty(info).expect("serialize test daemon record");
    std::fs::write(state_dir.join("daemon.json"), &bytes).expect("write test daemon record");
    bytes
}

/// A reply script for [`FakeHealthPeer`] that behaves like a daemon: an
/// identified `PONG`, the given record as `STATUS`, and `STOPPING`.
fn scripted_daemon(info: &DaemonInfo) -> impl Fn(&str) -> Vec<u8> + Send + 'static {
    let status = format!(
        "{}\n",
        serde_json::to_string(info).expect("serialize test status")
    );
    move |command| match command {
        "PING" => pong_line().into_bytes(),
        "STATUS" => status.clone().into_bytes(),
        "STOP" => b"STOPPING\n".to_vec(),
        _ => b"ERR unknown command\n".to_vec(),
    }
}

/// [`scripted_daemon`] that also raises `stop_seen` the moment it is sent `STOP`,
/// so a test can release the daemon lock only after the CLI really asked.
fn scripted_daemon_noting_stop(
    info: &DaemonInfo,
    stop_seen: Arc<AtomicBool>,
) -> impl Fn(&str) -> Vec<u8> + Send + 'static {
    let inner = scripted_daemon(info);
    move |command| {
        if command == "STOP" {
            stop_seen.store(true, Ordering::SeqCst);
        }
        inner(command)
    }
}

/// Block until `flag` is raised, failing the test after `budget`.
fn wait_for_flag(flag: &AtomicBool, budget: Duration) {
    let started = Instant::now();
    while !flag.load(Ordering::SeqCst) {
        assert!(
            started.elapsed() < budget,
            "flag not raised within {budget:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Name of the file whose presence proves a daemon lock was taken.
#[cfg(unix)]
fn control_lock_name() -> &'static str {
    hyperdb_mcp::daemon::lock::LOCK_FILE_NAME
}

/// Remove the "private to this user" property from a state directory.
#[cfg(unix)]
fn make_group_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o770))
        .expect("make test state dir group-writable");
}

/// Read one line (including its newline) a byte at a time, so no buffered
/// reader swallows bytes meant for a later read. An empty string means EOF
/// before any byte.
fn read_line_from(stream: &mut ControlStream) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0_u8];
    loop {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    String::from_utf8(line)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// A scripted stand-in for a daemon: serves a control endpoint, records every
/// command it is sent, and answers each with whatever the script returns.
/// Drives the code under test without a real daemon or `hyperd`.
struct FakeHealthPeer {
    endpoint: HealthEndpoint,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Vec<String>>>,
}

impl FakeHealthPeer {
    /// Serve `<state_dir>/daemon.sock`.
    fn start_in(state_dir: &Path, reply: impl Fn(&str) -> Vec<u8> + Send + 'static) -> Self {
        let endpoint = HealthEndpoint::for_new_daemon(state_dir).expect("derive peer endpoint");
        Self::start_at(&endpoint, reply)
    }

    /// Serve an already derived endpoint.
    fn start_at(
        endpoint: &HealthEndpoint,
        reply: impl Fn(&str) -> Vec<u8> + Send + 'static,
    ) -> Self {
        let mut listener = ControlListener::bind(endpoint).expect("bind fake health peer");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut commands = Vec::new();
            while !thread_stop.load(Ordering::SeqCst) {
                let mut stream = match listener.accept_timeout(Duration::from_millis(50)) {
                    Ok(Some(stream)) => stream,
                    Ok(None) => continue,
                    Err(error) => panic!("fake health peer accept failed: {error}"),
                };
                if stream.set_io_timeout(Duration::from_secs(2)).is_err() {
                    continue;
                }
                let Ok(line) = read_line_from(&mut stream) else {
                    continue;
                };
                let command = line.trim();
                // A bare connect-and-hang-up is a wake-up, not a command.
                if command.is_empty() {
                    continue;
                }
                let response = reply(command);
                commands.push(command.to_string());
                let _ = stream.write_all(&response);
                let _ = stream.finish();
            }
            commands
        });
        Self {
            endpoint: endpoint.clone(),
            stop,
            handle: Some(handle),
        }
    }

    /// Stop serving and return every command received, in order.
    fn stop_and_join(&mut self) -> Vec<String> {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().map_or_else(Vec::new, |handle| {
            handle.join().expect("fake health peer must join cleanly")
        })
    }
}

impl Drop for FakeHealthPeer {
    fn drop(&mut self) {
        if self.handle.is_some() {
            let _ = self.stop_and_join();
        }
    }
}

/// A real [`HealthListener`] running on its own thread against a real
/// [`DaemonState`], bound inside a caller-owned state directory.
struct RunningHealth {
    endpoint: HealthEndpoint,
    state: Arc<DaemonState>,
    info: Arc<Mutex<DaemonInfo>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RunningHealth {
    fn start_with_info(
        state_dir: &Path,
        make_info: impl FnOnce(&HealthEndpoint) -> DaemonInfo,
    ) -> Self {
        let endpoint = HealthEndpoint::for_new_daemon(state_dir).expect("derive health endpoint");
        let listener = HealthListener::bind(&endpoint).expect("bind running health listener");
        let state = Arc::new(DaemonState::new(endpoint.clone()));
        let info = Arc::new(Mutex::new(make_info(&endpoint)));
        let run_state = Arc::clone(&state);
        let run_info = Arc::clone(&info);
        let handle = std::thread::spawn(move || listener.run(run_state, run_info));
        Self {
            endpoint,
            state,
            info,
            handle: Some(handle),
        }
    }

    /// Wait for the listener thread to return (after a `STOP`, say).
    fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .expect("running health listener must shut down cleanly");
        }
    }

    fn stop_and_join(&mut self) {
        self.state.request_shutdown();
        self.join();
    }
}

impl Drop for RunningHealth {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Starts a health listener in `state_dir`. Does NOT touch env vars — safe for
/// parallel use. The caller owns (and must outlive) the directory.
fn start_health_listener_in(state_dir: &Path) -> RunningHealth {
    RunningHealth::start_with_info(state_dir, test_daemon_info)
}

/// [`start_health_listener_in`] in a fresh short state directory.
fn start_health_listener() -> (TempDir, RunningHealth) {
    let dir = TempDir::new().expect("create short per-test state dir");
    let health = start_health_listener_in(dir.path());
    (dir, health)
}

/// Run `hyperdb-mcp <args>` against `state_dir` with a scrubbed environment and
/// a deadline. The child never sees the developer's real `HOME` or state
/// directory: both are pinned to `state_dir`, so a regression's blast radius
/// stays inside the test. A child that outlives `deadline` is killed and the
/// test fails with its output.
fn run_cli_bounded(args: &[&str], state_dir: &Path, deadline: Duration) -> Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hyperdb-mcp"));
    command.env_clear();
    preserve_child_runtime_environment(&mut command);
    command
        .current_dir(state_dir)
        .env("HOME", state_dir)
        .env("USERPROFILE", state_dir)
        .env("HYPERDB_STATE_DIR", state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    if let Some(hyperd) = std::env::var_os("HYPERD_PATH") {
        command.env("HYPERD_PATH", hyperd);
    }
    let mut child = command.spawn().expect("spawn hyperdb-mcp CLI child");
    let give_up_at = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .expect("collect completed CLI child output");
            }
            Ok(None) if Instant::now() < give_up_at => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let kill_error = child.kill().err();
                let output = child
                    .wait_with_output()
                    .expect("wait for timed-out CLI child after kill");
                panic!(
                    "`hyperdb-mcp {}` exceeded {deadline:?} and was killed ({kill_error:?})\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error) => {
                let kill_error = child.kill().err();
                let output = child
                    .wait_with_output()
                    .expect("wait for CLI child after status error");
                panic!(
                    "CLI child status failed: {error}; kill result: {kill_error:?}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

fn preserve_child_runtime_environment(command: &mut std::process::Command) {
    for key in [
        "PATH",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

/// Run `run_daemon` on its own thread and runtime, for the state directory
/// named by `HYPERDB_STATE_DIR`. Caller MUST hold `ENV_LOCK`.
fn spawn_daemon_thread() -> std::thread::JoinHandle<Result<(), String>> {
    std::thread::spawn(move || -> Result<(), String> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| format!("rt: {e}"))?;
        rt.block_on(async {
            let config = hyperdb_mcp::daemon::run::DaemonConfig {
                idle_timeout: Some(Duration::from_secs(300)),
            };
            hyperdb_mcp::daemon::run::run_daemon(config)
                .await
                .map_err(|e| format!("run_daemon: {e}"))
        })
    })
}

/// Wait up to `budget` for a daemon in `state_dir` whose `started_at` differs
/// from `not_started_at` to become discoverable. Panics with the thread's
/// error if `handle`'s daemon exits first.
fn wait_for_new_daemon(
    handle: &std::thread::JoinHandle<Result<(), String>>,
    state_dir: &Path,
    not_started_at: Option<&str>,
    budget: Duration,
) -> DaemonInfo {
    let start = Instant::now();
    loop {
        if let Some(info) = discovery::discover_in(state_dir)
            && Some(info.started_at.as_str()) != not_started_at
        {
            return info;
        }
        assert!(
            !handle.is_finished(),
            "the daemon thread exited before it became discoverable"
        );
        assert!(
            start.elapsed() <= budget,
            "no new daemon became discoverable within {budget:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A real daemon running in a background thread for integration tests.
/// Sets `HYPERDB_STATE_DIR` to an isolated, short directory.
/// Caller MUST hold `ENV_LOCK` before calling `start()`.
struct TestDaemon {
    info: DaemonInfo,
    endpoint: HealthEndpoint,
    state_dir: PathBuf,
    _state_dir_guard: EnvGuard,
    /// Declared last so the directory outlives every other field's drop.
    _tmp: TempDir,
}

impl TestDaemon {
    fn start() -> Self {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().to_path_buf();

        let state_dir_guard = EnvGuard::set("HYPERDB_STATE_DIR", state_dir.to_str().unwrap());

        // Start the daemon in a background tokio runtime. Capture errors
        // via a JoinHandle so a bind/spawn failure fails the test fast
        // with a real message instead of a 30s timeout. The daemon takes the
        // state directory from `HYPERDB_STATE_DIR` and serves
        // `<state_dir>/daemon.sock`; there is no port to choose or race for.
        let daemon_handle = std::thread::spawn(move || -> Result<(), String> {
            let rt = tokio::runtime::Runtime::new().map_err(|e| format!("rt: {e}"))?;
            rt.block_on(async {
                let config = hyperdb_mcp::daemon::run::DaemonConfig {
                    idle_timeout: Some(Duration::from_secs(300)),
                };
                hyperdb_mcp::daemon::run::run_daemon(config)
                    .await
                    .map_err(|e| format!("run_daemon: {e}"))
            })
        });

        // Wait for daemon to become ready. CI runners (especially macOS)
        // can be significantly slower than local dev — hyperd startup
        // alone may take 10+ seconds under load.
        //
        // The outer timeout MUST exceed `HyperProcess::new`'s internal
        // 60s `wait_for_callback` timeout. If `cmd.spawn()` itself is
        // delayed by CI resource contention, the 60s countdown doesn't
        // start until the process is actually running. 150s gives room
        // for spawn latency + the full callback timeout + tokio runtime
        // teardown, so the daemon thread can surface a real error
        // through `daemon_handle.is_finished()` rather than the test
        // hitting this generic timeout first.
        let start = Instant::now();
        loop {
            if let Some(info) = discovery::discover() {
                let endpoint = HealthEndpoint::from_record(&info.health_endpoint, &state_dir)
                    .expect("a discovered record names this state directory's endpoint");
                return Self {
                    info,
                    endpoint,
                    state_dir,
                    _state_dir_guard: state_dir_guard,
                    _tmp: tmp,
                };
            }
            // Fail fast if the daemon thread has already exited (bind error,
            // spawn error, etc.). Avoids the unhelpful generic-timeout panic.
            if daemon_handle.is_finished() {
                let msg = match daemon_handle.join() {
                    Ok(Ok(())) => "daemon thread exited cleanly without writing discovery".into(),
                    Ok(Err(e)) => format!("daemon thread errored: {e}"),
                    Err(_) => "daemon thread panicked".into(),
                };
                panic!("TestDaemon failed to start: {msg}");
            }
            assert!(
                start.elapsed() <= Duration::from_secs(150),
                "TestDaemon did not start within 150s (daemon thread still running, no discovery file written)"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = health::send_command(&self.endpoint, "STOP");
        // The daemon holds the state directory's lock until after its `hyperd`
        // has exited, so the lock being free is the one reliable sign of full
        // shutdown (`HyperProcess` drop can take ~5s). Waiting on the control
        // endpoint instead would return before `hyperd` is gone, and the next
        // test could find the previous one's engine still winding down.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match DaemonLock::try_acquire(&self.state_dir) {
                Ok(Some(lock)) => {
                    drop(lock);
                    return;
                }
                // Cannot be checked at all (directory gone): nothing to wait on.
                Err(_) => return,
                Ok(None) => {}
            }
            if Instant::now() >= deadline {
                let message = format!(
                    "TestDaemon: the daemon lock in {} was still held 15s after STOP",
                    self.state_dir.display()
                );
                if std::thread::panicking() {
                    eprintln!("{message}");
                    return;
                }
                panic!("{message}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Locate the `hyperd` process serving a daemon endpoint via `lsof`. Over IPC
/// the endpoint is a Unix domain socket path (`<state>/sockets/hyper`), so
/// match the process holding that socket file open; for a TCP `host:port`
/// endpoint, match the process listening on the port. Returns the PID of
/// whichever process owns it. Unix-only.
#[cfg(unix)]
fn find_hyperd_pid_for_endpoint(endpoint: &str) -> Option<u32> {
    use std::process::Command;

    // `lsof -t` prints just the PID(s). A socket path is passed as a
    // positional file argument; a TCP endpoint selects the listening socket by
    // port.
    //
    // On the socket-path branch, `-a` is load-bearing (#310). lsof combines
    // separately-stated selectors with OR, not AND, unless `-a` is given — so
    // `-c hyperd <path>` means "every process named hyperd, OR anything holding
    // <path> open", i.e. the PID of *every* concurrent hyperd, not the one at
    // this socket. Under the parallel test binary that OR union made
    // `.find(first)` pick an unrelated test's hyperd; killing it left this
    // test's hyperd alive, so the daemon's monitor never restarted and
    // `wait_for_live_hyperd_after_kill` timed out — reproducibly on the loaded
    // ubuntu runner, never in the serial/macOS runs. `-a` ANDs the command-name
    // and path selectors so only the hyperd actually bound to `<path>` matches.
    // (The TCP branch was never affected: `-iTCP:{port} -sTCP:LISTEN` already
    // names a unique listener, which is why the tests passed on TCP `main`.)
    let output = if endpoint.starts_with('/') {
        Command::new("lsof")
            .args(["-nP", "-t", "-a", "-c", "hyperd", endpoint])
            .output()
    } else {
        let port = endpoint.rsplit(':').next()?.parse::<u16>().ok()?;
        Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
            .output()
    }
    .ok()?;
    if !output.status.success() {
        return None;
    }
    output
        .stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .map(str::trim)
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse::<u32>().ok())
}

/// Whether `endpoint` currently accepts a connection, over whichever transport
/// it names: a Unix domain socket path (`/…`) or a TCP `host:port`. Mirrors how
/// a client would reach it, so the restart tests can gate on real reachability
/// regardless of the daemon's transport. Unix-only.
#[cfg(unix)]
fn endpoint_accepts_connection(endpoint: &str) -> bool {
    if endpoint.starts_with('/') {
        std::os::unix::net::UnixStream::connect(endpoint).is_ok()
    } else if let Ok(addr) = endpoint.parse::<std::net::SocketAddr>() {
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
    } else {
        false
    }
}

/// Kill the given PID with SIGKILL. Unix-only.
#[cfg(unix)]
fn kill_pid(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill -9 should run");
    assert!(status.success(), "kill -9 {pid} failed");
}

/// Wait for the daemon to advertise a *live* hyperd after the process serving
/// `killed_endpoint` was killed, and return the endpoint it settled on, or
/// `None` if the timeout expires.
///
/// Asking only "is the endpoint STATUS reports connectable?" is not enough,
/// and was the cause of a ~20%-per-run failure on this file's restart tests.
/// For a few milliseconds after `SIGKILL` the doomed hyperd's listening socket
/// still completes inbound connections, so a single-phase gate polled
/// immediately after the kill returns on its first attempt — carrying the
/// *pre-kill* endpoint, because the daemon's 5s monitor tick has not fired and
/// STATUS still advertises the old process. The caller then raced the kernel
/// tearing that socket down and failed with `Connection refused` a few
/// milliseconds later.
///
/// So gate on the old hyperd being observably gone first, then on the
/// advertised endpoint being connectable. That ordering is also why this
/// cannot simply require the endpoint to *change*: the OS is free to hand the
/// replacement the same port, and a changed-endpoint gate would then wait out
/// its whole timeout on a perfectly healthy restart.
#[cfg(unix)]
fn wait_for_live_hyperd_after_kill(
    health_endpoint: &HealthEndpoint,
    killed_endpoint: &str,
    timeout_secs: u64,
) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    // Phase 1: the killed hyperd must stop accepting connections. Until it
    // does, any endpoint we observe could still be its soon-to-close socket.
    // For the Unix socket the path is fixed across restarts, but the daemon's
    // monitor only reacts on its ~5s tick, so there is always a downtime window
    // in which the path refuses a connection before the replacement binds it.
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        if !endpoint_accepts_connection(killed_endpoint) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // Phase 2: whatever STATUS advertises now must be reachable. With the old
    // socket proven down, a successful connect means a live replacement.
    while Instant::now() < deadline {
        if let Ok(response) = health::send_command(health_endpoint, "STATUS")
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(response.trim())
            && let Some(endpoint) = parsed["hyperd_endpoint"].as_str()
            && endpoint_accepts_connection(endpoint)
        {
            return Some(endpoint.to_string());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// RAII guard that sets/removes an environment variable and restores it on drop.
struct EnvGuard {
    key: String,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        // SAFETY: every env mutation in this binary goes through EnvGuard under
        // ENV_LOCK, so writes never race each other or the locked tests that
        // read these keys. Tests that skip the lock read env only through std,
        // which serialises its own accesses against set_var.
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            previous,
        }
    }

    fn remove(key: &str) -> Self {
        let previous = std::env::var(key).ok();
        // SAFETY: see EnvGuard::set; all env mutation is serialised by ENV_LOCK.
        unsafe { std::env::remove_var(key) };
        Self {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            // SAFETY: see EnvGuard::set; the guard's owner holds ENV_LOCK.
            Some(val) => unsafe { std::env::set_var(&self.key, val) },
            // SAFETY: see EnvGuard::set; the guard's owner holds ENV_LOCK.
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

// ─── Takeover: a successor starts right behind a STOP ─────────────────────────

/// Two daemons in sequence on ONE state directory. Daemon B is started the
/// instant `STOP` has been sent to daemon A, while A's `hyperd` is still
/// winding down. B must wait out A (lock, then `hyperd` socket) within its
/// `STARTUP_WAIT`, become discoverable with a new `started_at`, serve a query
/// through its engine, and A's thread must exit cleanly. Without the
/// lock wait B failed on the lock or the `hyperd` socket pre-flight and no
/// daemon ran; without the prompt STOP, B is not discoverable within 3 s.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn a_daemon_started_right_after_stop_takes_over_the_state_directory() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let state_dir = tmp.path().to_path_buf();
    let _state_dir_guard = EnvGuard::set("HYPERDB_STATE_DIR", state_dir.to_str().unwrap());

    let daemon_a = spawn_daemon_thread();
    let info_a = wait_for_new_daemon(&daemon_a, &state_dir, None, Duration::from_secs(150));
    let endpoint = HealthEndpoint::from_record(&info_a.health_endpoint, &state_dir)
        .expect("a discovered record names this state directory's endpoint");

    // B must be discoverable well inside the old daemon's 5 s `hyperd_monitor`
    // poll (measured: ~0.3 s with the prompt STOP, ~5.2 s without it), so this
    // fails if STOP is only acted on at the next poll.
    let bound = Duration::from_secs(3);
    let exit_bound = Duration::from_secs(15);
    let started = Instant::now();
    let response = health::send_command(&endpoint, "STOP").expect("STOP reaches daemon A");
    assert_eq!(response.trim(), "STOPPING");
    let daemon_b = spawn_daemon_thread();

    let info_b = wait_for_new_daemon(&daemon_b, &state_dir, Some(&info_a.started_at), bound);
    assert_ne!(info_b.started_at, info_a.started_at);

    // B serves a query through an engine that reached it.
    let db = tmp.path().join("takeover.hyper");
    let engine = hyperdb_mcp::engine::Engine::new(Some(db.to_str().unwrap().to_string()))
        .expect("engine connects to daemon B");
    let engine_endpoint = engine
        .hyperd_endpoint()
        .expect("engine reports its hyperd endpoint")
        .clone();
    assert_eq!(
        engine_endpoint, info_b.hyperd_endpoint,
        "the engine must be in daemon mode against B, not local mode"
    );
    let rows = engine
        .execute_query_to_json("SELECT 1 AS one")
        .expect("daemon B serves a query");
    assert_eq!(rows.len(), 1);
    drop(engine);

    // A exited cleanly, within the bound.
    while !daemon_a.is_finished() {
        assert!(
            started.elapsed() <= exit_bound,
            "daemon A did not exit within {exit_bound:?} of STOP"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon_a
        .join()
        .expect("daemon A thread must not panic")
        .expect("daemon A must exit cleanly");

    // Tear B down and wait for its lock, as TestDaemon does.
    let endpoint_b = HealthEndpoint::from_record(&info_b.health_endpoint, &state_dir).unwrap();
    let _ = health::send_command(&endpoint_b, "STOP");
    hyperdb_mcp::daemon::spawn::wait_for_lock_release(&state_dir, Duration::from_secs(15))
        .expect("daemon B releases the lock after STOP");
    daemon_b
        .join()
        .expect("daemon B thread must not panic")
        .expect("daemon B must exit cleanly");
}

/// `STOP` is acted on at once: the record is gone within a second, not at the
/// next 5 s `hyperd` monitor poll. Measured on the record, because
/// `run_daemon` itself only returns after `hyperd` has been dropped.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"
)]
fn stop_removes_the_daemon_record_promptly() {
    let _lock = acquire_env_lock();
    let daemon = TestDaemon::start();
    let record = daemon.state_dir.join("daemon.json");
    assert!(record.exists(), "a running daemon has a record");

    let sent = Instant::now();
    health::send_command(&daemon.endpoint, "STOP").expect("STOP reaches the daemon");
    while record.exists() {
        assert!(
            sent.elapsed() < Duration::from_secs(1),
            "the record was still there {:?} after STOP",
            sent.elapsed()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
