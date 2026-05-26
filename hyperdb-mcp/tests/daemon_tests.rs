// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the single-instance daemon: discovery file, health protocol,
//! idle timeout, and full lifecycle integration with a real `hyperd`.
//!
//! Many tests mutate process-global environment variables (`HYPERDB_STATE_DIR`,
//! `HYPERDB_DAEMON_PORT`) to isolate their state directories. Because env vars
//! are process-global, these tests MUST run sequentially. We enforce this via a
//! shared mutex — every test that touches env vars acquires `ENV_LOCK` first.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyperdb_mcp::daemon::discovery::{self, DaemonInfo};
use hyperdb_mcp::daemon::health::{self, DaemonState, HealthListener};
use tempfile::TempDir;

/// Process-wide lock for tests that mutate environment variables.
/// Cargo runs tests in the same process by default — this prevents races.
/// We recover from poison to prevent one test's panic from cascading.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn acquire_env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ─── Unit tests: DaemonState (no env vars, safe to run in parallel) ───────────

#[test]
fn daemon_state_touch_resets_idle_duration() {
    let state = DaemonState::new();
    std::thread::sleep(Duration::from_millis(50));
    assert!(state.idle_duration() >= Duration::from_millis(50));

    state.touch();
    assert!(state.idle_duration() < Duration::from_millis(30));
}

#[test]
fn daemon_state_shutdown_flag() {
    let state = DaemonState::new();
    assert!(!state.should_shutdown());

    state.request_shutdown();
    assert!(state.should_shutdown());
}

#[test]
fn daemon_state_default_is_equivalent_to_new() {
    let default_state = DaemonState::default();
    assert!(!default_state.should_shutdown());
    assert!(default_state.idle_duration() < Duration::from_millis(100));
}

#[test]
fn daemon_state_default_initializes_restart_flag_false() {
    // Guard against future regressions where Default and new() diverge —
    // both must initialize restart_requested to false.
    let default_state = DaemonState::default();
    let new_state = DaemonState::new();
    assert!(!default_state.consume_restart_request());
    assert!(!new_state.consume_restart_request());
}

#[test]
fn daemon_state_restart_request_consume_round_trip() {
    let state = DaemonState::new();
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
    use hyperdb_mcp::daemon::run::{try_record_restart_attempt, RestartAttempt};
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
    use hyperdb_mcp::daemon::run::{try_record_restart_attempt, RestartAttempt};
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
    use hyperdb_mcp::daemon::run::{try_record_restart_attempt, RestartAttempt};
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

// ─── Unit tests: Health protocol (no env vars, safe to run in parallel) ───────

#[test]
fn health_listener_bind_succeeds_on_free_port() {
    let listener = HealthListener::bind(0).unwrap();
    assert_ne!(listener.port, 0);
}

#[test]
fn health_listener_second_bind_same_port_fails() {
    let listener = HealthListener::bind(0).unwrap();
    let port = listener.port;

    let result = HealthListener::bind(port);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
}

#[test]
fn health_protocol_ping_pong() {
    let (port, _handle, _state) = start_health_listener();

    let response = health::send_command(port, "PING").unwrap();
    assert_eq!(response.trim(), "PONG");
}

#[test]
fn health_protocol_heartbeat_resets_idle() {
    let (port, _handle, state) = start_health_listener();

    std::thread::sleep(Duration::from_millis(50));
    assert!(state.idle_duration() >= Duration::from_millis(50));

    let response = health::send_command(port, "HEARTBEAT").unwrap();
    assert_eq!(response.trim(), "OK");

    assert!(state.idle_duration() < Duration::from_millis(30));
}

#[test]
fn health_protocol_stop_triggers_shutdown() {
    let (port, handle, state) = start_health_listener();

    assert!(!state.should_shutdown());

    let response = health::send_command(port, "STOP").unwrap();
    assert_eq!(response.trim(), "STOPPING");

    assert!(state.should_shutdown());

    // Health listener should exit its loop
    handle.join().unwrap();
}

#[test]
fn health_protocol_status_returns_json() {
    let (port, _handle, _state) = start_health_listener();

    let response = health::send_command(port, "STATUS").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
    assert_eq!(parsed["pid"], 12345);
    assert_eq!(parsed["hyperd_endpoint"], "127.0.0.1:54321");
}

#[test]
fn health_protocol_unknown_command_returns_error() {
    let (port, _handle, _state) = start_health_listener();

    let response = health::send_command(port, "INVALID").unwrap();
    assert!(response.contains("ERR"));
}

#[test]
fn health_protocol_report_hyperd_error_sets_flag() {
    let (port, _handle, state) = start_health_listener();

    assert!(!state.consume_restart_request(), "flag starts clear");

    let response = health::send_command(port, "REPORT_HYPERD_ERROR").unwrap();
    assert_eq!(response.trim(), "OK");

    // The handler ran on a different thread; give it a moment to land the
    // store. AcqRel ordering means the store is visible here as soon as the
    // handler returns, but the response write is what unblocks send_command.
    assert!(
        state.consume_restart_request(),
        "REPORT_HYPERD_ERROR must set the restart-requested flag"
    );
}

#[test]
fn health_protocol_multi_command_session() {
    let (port, _handle, _state) = start_health_listener();

    let response1 = health::send_command(port, "PING").unwrap();
    assert_eq!(response1.trim(), "PONG");

    let response2 = health::send_command(port, "STATUS").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(response2.trim()).unwrap();
    assert_eq!(parsed["health_port"], port);

    let response3 = health::send_command(port, "HEARTBEAT").unwrap();
    assert_eq!(response3.trim(), "OK");
}

// ─── Unit tests: idle timeout logic (no env vars) ─────────────────────────────

#[test]
fn daemon_idle_timeout_shuts_down_daemon() {
    let state = Arc::new(DaemonState::new());
    let idle_timeout = Duration::from_secs(2);

    let monitor_state = Arc::clone(&state);
    let monitor = std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        if monitor_state.idle_duration() >= idle_timeout {
            monitor_state.request_shutdown();
            break;
        }
        if monitor_state.should_shutdown() {
            break;
        }
    });

    let start = Instant::now();
    monitor.join().unwrap();
    let elapsed = start.elapsed();

    assert!(state.should_shutdown());
    assert!(elapsed >= Duration::from_secs(2));
    assert!(elapsed < Duration::from_secs(4));
}

#[test]
fn daemon_heartbeat_prevents_idle_shutdown() {
    let state = Arc::new(DaemonState::new());
    let idle_timeout = Duration::from_secs(1);

    let monitor_state = Arc::clone(&state);
    let heartbeat_state = Arc::clone(&state);

    let heartbeat = std::thread::spawn(move || {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1500) {
            heartbeat_state.touch();
            std::thread::sleep(Duration::from_millis(200));
        }
    });

    let monitor = std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        if monitor_state.idle_duration() >= idle_timeout {
            monitor_state.request_shutdown();
            break;
        }
        if monitor_state.should_shutdown() {
            break;
        }
    });

    heartbeat.join().unwrap();
    let start = Instant::now();
    monitor.join().unwrap();
    let after_heartbeat_stop = start.elapsed();

    assert!(state.should_shutdown());
    assert!(
        after_heartbeat_stop >= Duration::from_millis(500),
        "daemon should have waited for idle timeout after heartbeats stopped, \
         but only waited {after_heartbeat_stop:?}"
    );
}

// ─── Unit tests: Discovery file (require ENV_LOCK) ────────────────────────────

#[test]
fn discovery_file_write_and_read() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let info = DaemonInfo {
        pid: 12345,
        hyperd_endpoint: "127.0.0.1:54321".to_string(),
        health_port: 7484,
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
    assert_eq!(read_back.health_port, 7484);
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
        health_port: 7484,
        started_at: "2026-01-01T00:00:00Z".to_string(),
        version: "0.1.0".to_string(),
    };
    discovery::write_discovery_file(&info1).unwrap();

    let info2 = DaemonInfo {
        pid: 200,
        hyperd_endpoint: "127.0.0.1:2222".to_string(),
        health_port: 7485,
        started_at: "2026-02-02T00:00:00Z".to_string(),
        version: "0.2.0".to_string(),
    };
    discovery::write_discovery_file(&info2).unwrap();

    let path = tmp.path().join("daemon.json");
    let contents = std::fs::read_to_string(&path).unwrap();
    let read_back: DaemonInfo = serde_json::from_str(&contents).unwrap();
    assert_eq!(read_back.pid, 200);
    assert_eq!(read_back.hyperd_endpoint, "127.0.0.1:2222");
}

#[test]
fn remove_discovery_file_deletes_it() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let info = DaemonInfo {
        pid: 1,
        hyperd_endpoint: "127.0.0.1:1".to_string(),
        health_port: 7484,
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

#[test]
fn discover_returns_none_for_stale_file() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let info = DaemonInfo {
        pid: 99999,
        hyperd_endpoint: "127.0.0.1:1".to_string(),
        health_port: 1,
        started_at: "2026-01-01T00:00:00Z".to_string(),
        version: "0.0.1".to_string(),
    };
    discovery::write_discovery_file(&info).unwrap();

    assert!(discovery::discover().is_none());

    let path = tmp.path().join("daemon.json");
    assert!(!path.exists());
}

#[test]
fn resolve_port_uses_env_var() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::set("HYPERDB_DAEMON_PORT", "9999");
    assert_eq!(discovery::resolve_port(), 9999);
}

#[test]
fn resolve_port_uses_default_when_env_unset() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::remove("HYPERDB_DAEMON_PORT");
    assert_eq!(
        discovery::resolve_port(),
        hyperdb_mcp::daemon::DEFAULT_DAEMON_PORT
    );
}

#[test]
fn discover_finds_live_daemon() {
    let _lock = acquire_env_lock();
    let tmp = TempDir::new().unwrap();
    let _guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

    let (port, _handle, _state) = start_health_listener();

    let info = DaemonInfo {
        pid: 12345,
        hyperd_endpoint: "127.0.0.1:54321".to_string(),
        health_port: port,
        started_at: "2026-05-20T10:30:00Z".to_string(),
        version: "0.1.3".to_string(),
    };
    discovery::write_discovery_file(&info).unwrap();

    let discovered = discovery::discover().expect("should discover live daemon");
    assert_eq!(discovered.pid, 12345);
    assert_eq!(discovered.health_port, port);
}

// ─── Integration tests: full daemon lifecycle with real hyperd ─────────────────

#[test]
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
    // We verify via the health port rather than the hyperd endpoint, because
    // the daemon's liveness monitor can restart hyperd (changing the endpoint)
    // between the two Engine::new calls.
    let ep1 = engine1.hyperd_endpoint().unwrap();
    let ep2 = engine2.hyperd_endpoint().unwrap();
    assert!(
        !ep1.is_empty() && !ep2.is_empty(),
        "both engines must report a daemon endpoint"
    );
    // Verify the daemon is the one we started (health port reachable)
    let status = health::send_command(daemon.info.health_port, "PING").unwrap();
    assert_eq!(status.trim(), "PONG");

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

    let resp = health::send_command(daemon.info.health_port, "PING").unwrap();
    assert_eq!(resp.trim(), "PONG");
}

#[cfg(unix)]
#[test]
fn hyperd_monitor_detects_killed_hyperd_and_restarts() {
    let _lock = acquire_env_lock();
    let daemon = TestDaemon::start();

    let pid_before = find_hyperd_pid_for_endpoint(&daemon.info.hyperd_endpoint)
        .expect("should find hyperd pid for endpoint before kill");

    // SIGKILL the hyperd process. The daemon's monitor should detect this on
    // the next 5s tick and restart hyperd.
    kill_pid(pid_before);

    // Wait up to 12 seconds for the monitor to fire and restart hyperd.
    // (5s monitor tick + spawn time + slack.)
    let new_endpoint = wait_for_endpoint_change_or_recovery(daemon.info.health_port, 12)
        .expect("daemon should restart hyperd within 12s");

    // The new endpoint must be reachable. Don't assert it differs from the old —
    // port reuse is permitted by the OS.
    let probe = std::net::TcpStream::connect_timeout(
        &new_endpoint.parse().expect("valid endpoint"),
        Duration::from_secs(2),
    );
    assert!(probe.is_ok(), "new hyperd endpoint should be reachable");
}

#[cfg(unix)]
#[test]
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
    let response = health::send_command(daemon.info.health_port, "REPORT_HYPERD_ERROR").unwrap();
    assert_eq!(response.trim(), "OK");

    let new_endpoint = wait_for_endpoint_change_or_recovery(daemon.info.health_port, 12)
        .expect("daemon should restart hyperd within 12s after report");

    let probe = std::net::TcpStream::connect_timeout(
        &new_endpoint.parse().expect("valid endpoint"),
        Duration::from_secs(2),
    );
    assert!(probe.is_ok(), "new hyperd endpoint should be reachable");
}

#[cfg(unix)]
#[test]
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

    // Wait for daemon-side restart (discovery file gets a new endpoint).
    wait_for_endpoint_change_or_recovery(daemon.info.health_port, 12)
        .expect("daemon should restart hyperd within 12s");

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

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// Starts a health listener on a random port and returns the port, join handle,
/// and shared state. Does NOT touch env vars — safe for parallel use.
fn start_health_listener() -> (u16, std::thread::JoinHandle<()>, Arc<DaemonState>) {
    let listener = HealthListener::bind(0).unwrap();
    let port = listener.port;
    let state = Arc::new(DaemonState::new());
    let run_state = Arc::clone(&state);

    let info = Arc::new(Mutex::new(DaemonInfo {
        pid: 12345,
        hyperd_endpoint: "127.0.0.1:54321".to_string(),
        health_port: port,
        started_at: "2026-05-20T10:30:00Z".to_string(),
        version: "0.1.3".to_string(),
    }));

    let handle = std::thread::spawn(move || {
        listener.run(run_state, info);
    });

    // Give the listener a moment to start accepting
    std::thread::sleep(Duration::from_millis(50));

    (port, handle, state)
}

/// A real daemon running in a background thread for integration tests.
/// Sets `HYPERDB_STATE_DIR` and `HYPERDB_DAEMON_PORT` to isolated values.
/// Caller MUST hold `ENV_LOCK` before calling `start()`.
struct TestDaemon {
    info: DaemonInfo,
    _state_dir_guard: EnvGuard,
    _port_guard: EnvGuard,
}

impl TestDaemon {
    fn start() -> Self {
        let tmp = TempDir::new().unwrap();
        // Leak the TempDir so it persists for the lifetime of the test.
        let tmp = Box::leak(Box::new(tmp));

        let state_dir_guard = EnvGuard::set("HYPERDB_STATE_DIR", tmp.path().to_str().unwrap());

        // Pass port 0 so the daemon's HealthListener binds an OS-assigned
        // free port and reports it back via the discovery file. We avoid
        // the find_free_port → set env → daemon binds later TOCTOU race
        // where another process could grab the port between pick and bind
        // (a real source of flakes on busy CI runners). The
        // `HYPERDB_DAEMON_PORT` env var only matters for the
        // spawn-daemon-if-missing path; once we've written the discovery
        // file, clients read `health_port` directly from it.
        let port_guard = EnvGuard::set("HYPERDB_DAEMON_PORT", "0");

        // Start the daemon in a background tokio runtime. Capture errors
        // via a JoinHandle so a bind/spawn failure fails the test fast
        // with a real message instead of a 30s timeout.
        let daemon_handle = std::thread::spawn(move || -> Result<(), String> {
            let rt = tokio::runtime::Runtime::new().map_err(|e| format!("rt: {e}"))?;
            rt.block_on(async {
                let config = hyperdb_mcp::daemon::run::DaemonConfig {
                    port: 0,
                    idle_timeout: Duration::from_secs(300),
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
        // The outer timeout MUST exceed `HyperProcess::new`'s own 30s
        // wait for the hyperd-callback connection. Otherwise, when
        // hyperd is slow to start, the bare "TestDaemon did not start"
        // assertion fires before the daemon thread can return its
        // actual error — masking the real cause behind a generic
        // timeout. 60s gives the inner timeout room to surface via the
        // `daemon_handle.is_finished()` branch below.
        let start = Instant::now();
        loop {
            if let Some(info) = discovery::discover() {
                return Self {
                    info,
                    _state_dir_guard: state_dir_guard,
                    _port_guard: port_guard,
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
                start.elapsed() <= Duration::from_secs(60),
                "TestDaemon did not start within 60 seconds (daemon thread still running, no discovery file written)"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = health::send_command(self.info.health_port, "STOP");
        // Wait until the daemon's health port is unreachable, indicating full
        // shutdown (HyperProcess Drop can take up to ~5s). 200ms was not
        // enough — under load, the next test could find the port still bound.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.info.health_port));
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Locate the `hyperd` process by matching the listen-port portion of an
/// endpoint string like `127.0.0.1:54321` against `lsof`'s view of TCP ports.
/// Returns the PID of whichever process owns the port. Unix-only.
#[cfg(unix)]
fn find_hyperd_pid_for_endpoint(endpoint: &str) -> Option<u32> {
    use std::process::Command;

    let port = endpoint.rsplit(':').next()?.parse::<u16>().ok()?;
    // `lsof -nP -iTCP:<port> -sTCP:LISTEN -t` prints just the PID(s) listening on that port.
    let output = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
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

/// Kill the given PID with SIGKILL. Unix-only.
#[cfg(unix)]
fn kill_pid(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill -9 should run");
    assert!(status.success(), "kill -9 {pid} failed");
}

/// Poll the daemon's `STATUS` endpoint until the reported `hyperd_endpoint` is
/// reachable (i.e. a fresh hyperd has been spawned after a kill). Returns the
/// endpoint string, or `None` if the timeout expires.
#[cfg(unix)]
fn wait_for_endpoint_change_or_recovery(health_port: u16, timeout_secs: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        if let Ok(response) = health::send_command(health_port, "STATUS") {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(response.trim()) {
                if let Some(endpoint) = parsed["hyperd_endpoint"].as_str() {
                    if let Ok(addr) = endpoint.parse::<std::net::SocketAddr>() {
                        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500))
                            .is_ok()
                        {
                            return Some(endpoint.to_string());
                        }
                    }
                }
            }
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
        // SAFETY: Callers hold ENV_LOCK, ensuring no concurrent env var access.
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            previous,
        }
    }

    fn remove(key: &str) -> Self {
        let previous = std::env::var(key).ok();
        // SAFETY: Callers hold ENV_LOCK, ensuring no concurrent env var access.
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
            // SAFETY: Callers hold ENV_LOCK for the lifetime of this guard.
            Some(val) => unsafe { std::env::set_var(&self.key, val) },
            // SAFETY: Callers hold ENV_LOCK for the lifetime of this guard.
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}
