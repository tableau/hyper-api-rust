// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the single-instance daemon: discovery file, health protocol,
//! idle timeout, and full lifecycle integration with a real `hyperd`.
//!
//! Many tests mutate process-global environment variables (`HYPERDB_STATE_DIR`,
//! `HYPERDB_DAEMON_PORT`) to isolate their state directories. Because env vars
//! are process-global, these tests MUST run sequentially. We enforce this via a
//! shared mutex — every test that touches env vars acquires `ENV_LOCK` first.

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyperdb_mcp::daemon::discovery::{self, DaemonInfo, PortScan};
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

// ─── Unit tests: DaemonConfig (require ENV_LOCK) ─────────────────────────────

#[test]
fn daemon_config_from_args_none_when_unset() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::remove("HYPERDB_DAEMON_IDLE_TIMEOUT");

    let config = hyperdb_mcp::daemon::run::DaemonConfig::from_args(0, None);
    assert!(
        config.idle_timeout.is_none(),
        "idle_timeout should be None when neither flag nor env is set"
    );
}

#[test]
fn daemon_config_from_args_some_when_flag() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::remove("HYPERDB_DAEMON_IDLE_TIMEOUT");

    let config = hyperdb_mcp::daemon::run::DaemonConfig::from_args(0, Some(120));
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

    let config = hyperdb_mcp::daemon::run::DaemonConfig::from_args(0, None);
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

    let config = hyperdb_mcp::daemon::run::DaemonConfig::from_args(0, Some(120));
    assert_eq!(
        config.idle_timeout,
        Some(Duration::from_secs(120)),
        "flag value should take precedence over env var"
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
    assert!(response.trim().starts_with("PONG hyperdb-mcp "));
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
    assert!(response1.trim().starts_with("PONG hyperdb-mcp "));

    let response2 = health::send_command(port, "STATUS").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(response2.trim()).unwrap();
    assert_eq!(parsed["health_port"], port);

    let response3 = health::send_command(port, "HEARTBEAT").unwrap();
    assert_eq!(response3.trim(), "OK");
}

#[test]
fn health_protocol_ping_identity_accept() {
    let (port, _handle, _state) = start_health_listener();

    let version =
        health::ping_identified(port, Duration::from_millis(300), Duration::from_millis(300))
            .expect("should return Some for a valid hyperdb-mcp daemon");
    assert_eq!(version, hyperdb_mcp::version::MCP_VERSION);
}

#[test]
fn health_protocol_ping_identity_reject_foreign() {
    use std::io::Write;

    // Bind a raw listener that returns a bare "PONG\n" without the identifying token
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.write_all(b"PONG\n");
        }
    });

    // Give the thread a moment to start accepting
    std::thread::sleep(Duration::from_millis(50));

    let result =
        health::ping_identified(port, Duration::from_millis(300), Duration::from_millis(300));
    assert_eq!(result, None, "should reject foreign PONG without token");
}

#[test]
fn health_protocol_ping_identity_reject_token_lookalike() {
    use std::io::Write;

    // A foreign service whose token *starts with* "hyperdb-mcp" must NOT pass.
    // Guards against a naive `starts_with("PONG hyperdb-mcp")` prefix check.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.write_all(b"PONG hyperdb-mcpEVIL 9.9.9\n");
        }
    });

    std::thread::sleep(Duration::from_millis(50));

    let result =
        health::ping_identified(port, Duration::from_millis(300), Duration::from_millis(300));
    assert_eq!(
        result, None,
        "must reject a token that only shares a prefix"
    );
}

#[test]
fn health_protocol_ping_identity_reject_refused() {
    // Bind a listener to get a port, then drop it so the port is closed
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    // Brief sleep to reduce the race (the OS may not have fully released the port)
    std::thread::sleep(Duration::from_millis(10));

    let result =
        health::ping_identified(port, Duration::from_millis(300), Duration::from_millis(300));
    assert_eq!(result, None, "should return None for connection refused");
}

#[test]
fn resolve_port_scan_pins_when_env_set() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::set("HYPERDB_DAEMON_PORT", "9001");
    assert_eq!(
        discovery::resolve_port_scan(),
        PortScan {
            base: 9001,
            span: 1
        }
    );
}

#[test]
fn resolve_port_scan_scans_when_env_unset() {
    let _lock = acquire_env_lock();
    let _guard = EnvGuard::remove("HYPERDB_DAEMON_PORT");
    let scan = discovery::resolve_port_scan();
    assert_eq!(scan.base, hyperdb_mcp::daemon::DEFAULT_DAEMON_BASE_PORT);
    assert_eq!(scan.span, hyperdb_mcp::daemon::DAEMON_PORT_SCAN_SPAN);
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
        hyperdb_mcp::daemon::DEFAULT_DAEMON_BASE_PORT
    );
}

// ─── Unit tests: Port scanning (require ENV_LOCK + sandbox OFF) ──────────────

#[test]
fn scan_finds_our_daemon_via_status() {
    let _lock = acquire_env_lock();
    let (port, _handle, _state) = start_health_listener();

    let scan = PortScan {
        base: port,
        span: 1,
    };
    match discovery::scan_for_daemon(scan) {
        discovery::ScanOutcome::Found(info) => {
            assert_eq!(info.hyperd_endpoint, "127.0.0.1:54321");
        }
        other => panic!("expected Found, got {other:?}"),
    }
}

#[test]
fn scan_skips_camped_returns_free() {
    let _lock = acquire_env_lock();

    // Find a camped port `base` whose immediate successor `base + 1` is free,
    // so the scan range is exactly the two adjacent ports {base, base+1}.
    //
    // TOCTOU mitigation: other tests' `start_health_listener` helpers leak
    // identity-answering listeners on random high ports for the test-process
    // lifetime. If one steals `base+1` between our probe-drop and the scan,
    // the scan returns `Found` instead of `FreePort`. We retry with a fresh
    // port pair up to 5 times to tolerate this race on busy CI runners.
    for _attempt in 0..5 {
        let (camped_listener, base) = loop {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            if port < u16::MAX {
                if let Ok(probe) = TcpListener::bind(("127.0.0.1", port + 1)) {
                    drop(probe);
                    break (listener, port);
                }
            }
            drop(listener);
        };
        let expected_free = base + 1;

        // Spawn a thread that keeps the camped listener alive and accepts
        // connections, answering with non-protocol garbage so the identity
        // check classifies it as `Camped`, not `OurDaemon`.
        std::thread::spawn(move || loop {
            if let Ok((mut stream, _)) = camped_listener.accept() {
                use std::io::Write;
                let _ = stream.write_all(b"NOPE\n");
            }
        });

        // Give the thread a moment to start accepting.
        std::thread::sleep(Duration::from_millis(50));

        // Scan exactly {base (camped), base+1 (free)}.
        let scan = PortScan { base, span: 2 };

        match discovery::scan_for_daemon(scan) {
            discovery::ScanOutcome::FreePort(port) => {
                assert_eq!(
                    port, expected_free,
                    "scan should skip the camped base port and return base+1"
                );
                return; // Success
            }
            discovery::ScanOutcome::Found(_) => {
                // Another test's leaked health listener stole base+1 — retry.
            }
            other => panic!("expected FreePort, got {other:?}"),
        }
    }
    panic!("scan_skips_camped_returns_free: failed after 5 attempts (port stolen each time)");
}

#[test]
fn scan_all_refused_returns_freeport_base() {
    // Pick a high port that's almost certainly free.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = listener.local_addr().unwrap().port();
    drop(listener); // Release the port so it's free again.

    std::thread::sleep(Duration::from_millis(10));

    // Span of 1: probe only the single known-free `base` port. A wider span
    // would risk colliding with a leaked health listener from another parallel
    // test (they bind random high ports and leak for the test-process lifetime),
    // which would be reported as `Found` rather than `FreePort`.
    let scan = PortScan { base, span: 1 };
    match discovery::scan_for_daemon(scan) {
        discovery::ScanOutcome::FreePort(port) => {
            assert_eq!(port, base, "should return the first free port (base)");
        }
        other => panic!("expected FreePort(base), got {other:?}"),
    }
}

#[test]
fn probe_refused_when_closed() {
    // Bind a listener to get a port, then drop it so the port is closed.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    std::thread::sleep(Duration::from_millis(10));

    // Access the private probe_port via the public scan_for_daemon wrapper.
    // We know that if the scan returns FreePort, then probe_port returned Refused.
    let scan = PortScan {
        base: port,
        span: 1,
    };
    match discovery::scan_for_daemon(scan) {
        discovery::ScanOutcome::FreePort(p) => {
            assert_eq!(
                p, port,
                "probe_port should have returned Refused for closed port"
            );
        }
        other => panic!("expected FreePort (Refused), got {other:?}"),
    }
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

// ─── Unit tests: concurrent-spawn dedup (no env vars, safe parallel) ────────────

#[test]
fn scan_for_daemon_prefers_lower_port_daemon() {
    // Simulates the concurrent-spawn race: two daemons ended up on adjacent ports
    // (base and base+1). The client that landed on base+1 should prefer the
    // base-port daemon when it re-scans the lower range.
    //
    // We model this by starting two real health listeners (which answer PING with
    // the identifying token), then asserting that scan_for_daemon with a 2-port
    // range returns the LOWER port's daemon as Found.
    let (lower_port, _lower_handle, _lower_state) = start_health_listener();
    let (higher_port, _higher_handle, _higher_state) = start_health_listener();

    // Make sure lower_port < higher_port for a predictable result.
    let (base, _top) = if lower_port < higher_port {
        (lower_port, higher_port)
    } else {
        (higher_port, lower_port)
    };

    let scan = PortScan { base, span: 2 };

    match discovery::scan_for_daemon(scan) {
        discovery::ScanOutcome::Found(info) => {
            assert_eq!(
                info.health_port, base,
                "scan should return the LOWEST-port daemon (the first one bound)"
            );
        }
        other => panic!("expected Found, got {other:?}"),
    }
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

// ─── Integration tests: full daemon lifecycle with real hyperd ─────────────────

#[test]
#[ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"]
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
#[ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"]
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
#[ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"]
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
#[ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"]
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
    assert!(resp.trim().starts_with("PONG hyperdb-mcp "));
}

#[cfg(unix)]
#[test]
#[ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"]
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
#[ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"]
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
#[ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"]
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
#[ignore = "flaky on macOS CI — daemon startup exceeds 150s timeout"]
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
                start.elapsed() <= Duration::from_secs(150),
                "TestDaemon did not start within 150s (daemon thread still running, no discovery file written)"
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
