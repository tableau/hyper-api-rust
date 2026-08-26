// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Regression tests for the Claude-session bug: `sample` returning a
//! spurious `TABLE_NOT_FOUND` due to a racy `has_table` probe, and the
//! connection-lost detection heuristics that drive auto-reconnect.

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use common::TestEngine;
use hyperdb_api::{HyperProcess, Parameters, TransportMode};
use hyperdb_mcp::daemon::discovery::{self, DaemonInfo};
use hyperdb_mcp::daemon::health;
use hyperdb_mcp::error::{is_connection_lost, ErrorCode};
use hyperdb_mcp::server::HyperMcpServer;

const SLOW_HEALTH_CHILD_ENV: &str = "HYPERDB_MCP_SLOW_HEALTH_CHILD";
const SLOW_HEALTH_CHILD_MODE_ENV: &str = "HYPERDB_MCP_SLOW_HEALTH_CHILD_MODE";
const SLOW_HEALTH_HYPER_PID_PATH_ENV: &str = "HYPERDB_MCP_SLOW_HEALTH_HYPER_PID_PATH";
const SLOW_HEALTH_TEST_NAME: &str = "slow_health_report_does_not_hold_engine_mutex";
const WATCHDOG_FAILURE_TEST_NAME: &str = "slow_health_watchdog_reaps_hyperd_after_child_failure";
const WATCHDOG_TIMEOUT_TEST_NAME: &str = "slow_health_watchdog_reaps_hyperd_after_child_timeout";
const CHILD_MODE_REGRESSION: &str = "regression";
const CHILD_MODE_FAIL_AFTER_HYPER_START: &str = "fail_after_hyper_start";
const CHILD_MODE_HANG_AFTER_HYPER_START: &str = "hang_after_hyper_start";

/// After creating a table and inserting rows, `sample_table` must return the
/// rows — not `TABLE_NOT_FOUND`. The old implementation used `has_table` with
/// `.unwrap_or(false)`, which silently returned false on any catalog read
/// hiccup.
#[test]
fn sample_works_immediately_after_insert() {
    let te = TestEngine::new_ephemeral();
    te.engine
        .execute_command("CREATE TABLE recent (id INT, label TEXT)")
        .unwrap();
    te.engine
        .execute_command("INSERT INTO recent VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();

    let sample = te.engine.sample_table("recent", 10).unwrap();
    assert_eq!(sample["table"], "recent");
    assert_eq!(sample["row_count"], 3);
    let rows = sample["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
}

/// `sample_table` on a missing table must return `TABLE_NOT_FOUND` (not the
/// underlying Hyper "does not exist (42P01)" message).
#[test]
fn sample_missing_table_translates_to_table_not_found() {
    let te = TestEngine::new_ephemeral();
    let err = te
        .engine
        .sample_table("this_table_does_not_exist", 5)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::TableNotFound);
    assert!(err.message.contains("this_table_does_not_exist"));
}

/// Classifier: recognizes the OS-level "broken pipe" error from a dead
/// hyperd, along with related transport-level failure strings.
#[test]
fn connection_lost_classifier_recognizes_transport_errors() {
    assert!(is_connection_lost("Broken pipe (os error 32)"));
    assert!(is_connection_lost("Connection reset by peer"));
    assert!(is_connection_lost("Connection refused"));
    assert!(is_connection_lost("connection closed"));
    assert!(is_connection_lost("unexpected EOF"));
    assert!(is_connection_lost(
        "server unexpectedly closed the connection"
    ));
    assert!(is_connection_lost("Socket is not connected"));
}

/// Classifier: does NOT flag ordinary SQL errors as transport errors. These
/// must keep their normal error code routing.
#[test]
fn connection_lost_classifier_ignores_sql_errors() {
    assert!(!is_connection_lost("syntax error at or near \"SELEKT\""));
    assert!(!is_connection_lost("table \"foo\" does not exist"));
    assert!(!is_connection_lost("column \"bar\" does not exist"));
    assert!(!is_connection_lost("ERROR: table already exists (42P07)"));
    assert!(!is_connection_lost(
        "ERROR: non-NULL value required (23502)"
    ));
    assert!(!is_connection_lost(""));
}

/// A slow daemon error report must not retain the server's engine mutex.
///
/// This runs the real reconnect/error-report path in a bounded self-child:
/// a real TCP `HyperProcess` supplies the data plane while a controlled,
/// OS-assigned listener supplies the daemon health plane. The listener holds
/// the `REPORT_HYPERD_ERROR` response until the test releases it. Once that
/// report is observed, a competing caller must acquire the public engine
/// handle before the report is released.
#[test]
fn slow_health_report_does_not_hold_engine_mutex() {
    match child_mode().as_deref() {
        Some(CHILD_MODE_REGRESSION) => run_slow_health_mutex_child(),
        Some(other) => panic!("unexpected child mode {other} for {SLOW_HEALTH_TEST_NAME}"),
        None => {
            let run =
                run_exact_child_with_watchdog(SLOW_HEALTH_TEST_NAME, CHILD_MODE_REGRESSION, None)
                    .expect("run bounded slow-health regression child");
            assert_exact_child_executed_one_test(&run.output);
            assert!(
                !run.timed_out,
                "slow-health regression child exceeded its 30s watchdog\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&run.output.stdout),
                String::from_utf8_lossy(&run.output.stderr)
            );
            assert!(
                run.output.status.success(),
                "slow-health child failed with {}; {}\nchild stdout:\n{}\nchild stderr:\n{}",
                run.output.status,
                run.cleanup,
                String::from_utf8_lossy(&run.output.stdout),
                String::from_utf8_lossy(&run.output.stderr)
            );
        }
    }
}

#[test]
fn slow_health_watchdog_reaps_hyperd_after_child_failure() {
    match child_mode().as_deref() {
        Some(CHILD_MODE_FAIL_AFTER_HYPER_START) => {
            run_fault_child(CHILD_MODE_FAIL_AFTER_HYPER_START)
        }
        Some(other) => panic!("unexpected child mode {other} for {WATCHDOG_FAILURE_TEST_NAME}"),
        None => {}
    }
    let run = run_exact_child_with_watchdog(
        WATCHDOG_FAILURE_TEST_NAME,
        CHILD_MODE_FAIL_AFTER_HYPER_START,
        None,
    )
    .expect("run intentional failing child through watchdog");
    assert_exact_child_executed_one_test(&run.output);
    assert!(!run.timed_out, "intentional failing child must exit itself");
    assert_eq!(
        run.output.status.code(),
        Some(23),
        "intentional failing child must preserve its sentinel exit code; {}\nstdout:\n{}\nstderr:\n{}",
        run.cleanup,
        String::from_utf8_lossy(&run.output.stdout),
        String::from_utf8_lossy(&run.output.stderr)
    );
}

#[test]
fn slow_health_watchdog_reaps_hyperd_after_child_timeout() {
    match child_mode().as_deref() {
        Some(CHILD_MODE_HANG_AFTER_HYPER_START) => {
            run_fault_child(CHILD_MODE_HANG_AFTER_HYPER_START)
        }
        Some(other) => panic!("unexpected child mode {other} for {WATCHDOG_TIMEOUT_TEST_NAME}"),
        None => {}
    }
    let run = run_exact_child_with_watchdog(
        WATCHDOG_TIMEOUT_TEST_NAME,
        CHILD_MODE_HANG_AFTER_HYPER_START,
        Some(Duration::from_millis(250)),
    )
    .expect("run intentional hanging child through watchdog");
    assert_exact_child_executed_one_test(&run.output);
    assert!(
        run.timed_out,
        "intentional hanging child must take the watchdog timeout branch; {}\nstdout:\n{}\nstderr:\n{}",
        run.cleanup,
        String::from_utf8_lossy(&run.output.stdout),
        String::from_utf8_lossy(&run.output.stderr)
    );
}

fn child_mode() -> Option<String> {
    std::env::var_os(SLOW_HEALTH_CHILD_ENV)?;
    std::env::var(SLOW_HEALTH_CHILD_MODE_ENV).ok()
}

struct ChildRun {
    output: Output,
    timed_out: bool,
    cleanup: HyperCleanup,
}

#[derive(Debug)]
struct HyperCleanup {
    pid: u32,
    actively_terminated: bool,
}

impl std::fmt::Display for HyperCleanup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "reported Hyper PID {} stopped (active termination: {})",
            self.pid, self.actively_terminated
        )
    }
}

fn run_exact_child_with_watchdog(
    test_name: &str,
    mode: &str,
    timeout_after_pid_report: Option<Duration>,
) -> Result<ChildRun, String> {
    let temp = tempfile::TempDir::new().expect("create isolated child directory");
    let state_dir = temp.path().join("state");
    let process_temp_dir = temp.path().join("tmp");
    let hyper_pid_path = temp.path().join("hyperd.pid");
    std::fs::create_dir_all(&state_dir)
        .map_err(|error| format!("create isolated child state: {error}"))?;
    std::fs::create_dir_all(&process_temp_dir)
        .map_err(|error| format!("create isolated child temp: {error}"))?;

    let mut command = Command::new(
        std::env::current_exe().map_err(|error| format!("locate recovery test binary: {error}"))?,
    );
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .current_dir(temp.path())
        .env(SLOW_HEALTH_CHILD_ENV, "1")
        .env(SLOW_HEALTH_CHILD_MODE_ENV, mode)
        .env(SLOW_HEALTH_HYPER_PID_PATH_ENV, &hyper_pid_path)
        .env("HOME", &state_dir)
        .env("USERPROFILE", &state_dir)
        .env("HYPERDB_STATE_DIR", &state_dir)
        .env("TMPDIR", &process_temp_dir)
        .env("TEMP", &process_temp_dir)
        .env("TMP", &process_temp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_child_containment(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn exact {test_name} child: {error}"))?;

    let overall_deadline = Instant::now() + Duration::from_secs(30);
    let mut post_pid_deadline = None;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) => {
                if post_pid_deadline.is_none() && hyper_pid_path.is_file() {
                    post_pid_deadline =
                        timeout_after_pid_report.map(|timeout| Instant::now() + timeout);
                }
                let deadline = post_pid_deadline.unwrap_or(overall_deadline);
                if Instant::now() >= deadline {
                    break true;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let termination = terminate_child_tree(&mut child);
                let output = child
                    .wait_with_output()
                    .map_err(|wait_error| format!("reap child after status error: {wait_error}"))?;
                let cleanup = stop_reported_hyperd(&hyper_pid_path);
                return Err(format!(
                    "{test_name} child status failed: {error}; termination={termination:?}; cleanup={cleanup:?}\nchild stdout:\n{}\nchild stderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
    };

    if timed_out {
        terminate_child_tree(&mut child)?;
    }

    let output = child
        .wait_with_output()
        .map_err(|error| format!("reap completed {test_name} child: {error}"))?;
    // This runs for success, normal failure, and watchdog termination. A
    // nonzero child can never bypass active cleanup of the exact reported PID.
    let cleanup = stop_reported_hyperd(&hyper_pid_path)?;
    Ok(ChildRun {
        output,
        timed_out,
        cleanup,
    })
}

fn assert_exact_child_executed_one_test(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("running 1 test"),
        "exact child filter executed zero or multiple tests\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn start_reported_hyper_process(state_dir: &Path, hyper_pid_path: &Path) -> (HyperProcess, String) {
    let log_dir = state_dir.join("hyper-logs");
    std::fs::create_dir_all(&log_dir).expect("create isolated Hyper log directory");
    let mut parameters = Parameters::new();
    parameters.set_transport_mode(TransportMode::Tcp);
    parameters.set("log_dir", log_dir.to_string_lossy().as_ref());
    let hyper = HyperProcess::new(None, Some(&parameters)).expect("start real TCP HyperProcess");
    assert_eq!(hyper.transport_mode(), TransportMode::Tcp);
    let hyper_pid = hyper.pid().expect("HyperProcess must own a child PID");
    let endpoint = hyper
        .require_endpoint()
        .expect("TCP HyperProcess must publish an endpoint")
        .to_string();
    std::fs::write(hyper_pid_path, hyper_pid.to_string()).expect("report exact Hyper PID");
    (hyper, endpoint)
}

fn run_fault_child(mode: &str) -> ! {
    let state_dir = std::env::var_os("HYPERDB_STATE_DIR")
        .map(PathBuf::from)
        .expect("parent must provide an isolated fault-child state directory");
    let hyper_pid_path = std::env::var_os(SLOW_HEALTH_HYPER_PID_PATH_ENV)
        .map(PathBuf::from)
        .expect("parent must provide a fault-child Hyper PID report path");
    let (hyper, _endpoint) = start_reported_hyper_process(&state_dir, &hyper_pid_path);
    // Deliberately bypass RAII so these branches prove the parent watchdog's
    // containment and exact-PID cleanup rather than HyperProcess::drop.
    std::mem::forget(hyper);
    match mode {
        CHILD_MODE_FAIL_AFTER_HYPER_START => std::process::exit(23),
        CHILD_MODE_HANG_AFTER_HYPER_START => loop {
            thread::park_timeout(Duration::from_secs(60));
        },
        other => panic!("unsupported fault-child mode {other}"),
    }
}

#[cfg(unix)]
fn configure_child_containment(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    command.process_group(0);
}

#[cfg(windows)]
fn configure_child_containment(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn configure_child_containment(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_child_tree(child: &mut Child) -> Result<(), String> {
    let process_group = i32::try_from(child.id())
        .map_err(|error| format!("child PID does not fit process-group ID: {error}"))?;
    // SAFETY: the exact child was spawned into a new process group whose ID is
    // its validated PID. A negative target addresses only that contained group.
    let result = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("kill contained child process group: {error}"));
        }
    }
    let _ = child.kill();
    Ok(())
}

#[cfg(windows)]
fn terminate_child_tree(child: &mut Child) -> Result<(), String> {
    let output = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("run taskkill for contained child tree: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "taskkill child tree exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let _ = child.kill();
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn terminate_child_tree(child: &mut Child) -> Result<(), String> {
    child
        .kill()
        .map_err(|error| format!("kill timed-out child: {error}"))
}

type EngineHandle = Arc<Mutex<Option<hyperdb_mcp::engine::Engine>>>;

#[derive(Debug)]
struct ReportObservation {
    sequence: usize,
    engine_mutex_available: bool,
}

fn run_slow_health_mutex_child() {
    let state_dir = std::env::var_os("HYPERDB_STATE_DIR")
        .map(PathBuf::from)
        .expect("parent must provide an isolated daemon state directory");
    let hyper_pid_path = std::env::var_os(SLOW_HEALTH_HYPER_PID_PATH_ENV)
        .map(PathBuf::from)
        .expect("parent must provide a Hyper PID report path");
    let (hyper, endpoint) = start_reported_hyper_process(&state_dir, &hyper_pid_path);

    let listener =
        TcpListener::bind(("127.0.0.1", 0)).expect("bind controlled OS-assigned health listener");
    let health_port = listener
        .local_addr()
        .expect("read controlled health listener address")
        .port();

    let daemon_info = DaemonInfo {
        pid: std::process::id(),
        hyperd_endpoint: endpoint,
        health_port,
        started_at: "2026-08-14T00:00:00Z".to_string(),
        version: hyperdb_mcp::version::MCP_VERSION.to_string(),
    };
    // Discovery is the only routing input: Task 5 must carry this effective
    // health port through the engine and into the loss-report path. The child
    // intentionally does not mutate the process-global daemon-port setting.
    discovery::write_discovery_file(&daemon_info).expect("write isolated daemon discovery");

    let engine_probe = Arc::new(OnceLock::<EngineHandle>::new());
    let (report_seen_tx, report_seen_rx) = mpsc::channel();
    let (report_release_tx, report_release_rx) = mpsc::channel();
    let peer_info = daemon_info.clone();
    let peer_engine_probe = Arc::clone(&engine_probe);
    let peer = thread::spawn(move || {
        run_controlled_health_peer(
            &listener,
            &peer_info,
            &peer_engine_probe,
            &report_seen_tx,
            &report_release_rx,
        )
    });

    // A real protocol round-trip proves the listener is accepting and its
    // STATUS response is usable; no timing sleep is needed for readiness.
    let status = health::send_command_with_timeout(
        health_port,
        "STATUS",
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect("controlled health peer must answer STATUS");
    let status_info: DaemonInfo =
        serde_json::from_str(status.trim()).expect("controlled STATUS must be DaemonInfo JSON");
    assert_eq!(status_info, daemon_info);

    let server = Arc::new(HyperMcpServer::with_no_daemon(None, false, false));
    server.warm_up_engine();
    assert!(
        server
            .resource_body_for_uri("hyper://workspace")
            .expect("prime workspace resource through with_engine")
            .is_some(),
        "workspace resource must exist"
    );
    let engine_handle = server.engine_handle();
    engine_probe
        .set(Arc::clone(&engine_handle))
        .unwrap_or_else(|_| panic!("engine probe handle must be installed exactly once"));
    assert_eq!(
        engine_handle
            .lock()
            .expect("inspect warmed engine")
            .as_ref()
            .expect("warm-up must install an engine")
            .daemon_health_port(),
        Some(health_port),
        "warmed engine must retain the discovered health port"
    );

    hyper
        .shutdown_timeout(Duration::from_secs(5))
        .expect("shut down the real HyperProcess before inducing ConnectionLost");

    let mut failures = Vec::new();
    let worker_server = Arc::clone(&server);
    let loss_worker = thread::spawn(move || -> Result<ErrorCode, String> {
        match worker_server.resource_body_for_uri("hyper://workspace") {
            Err(error) => Ok(error.code),
            Ok(value) => Err(format!(
                "dead Hyper connection unexpectedly returned resource {value:?}"
            )),
        }
    });

    let first_report =
        receive_and_release_report(&report_seen_rx, &report_release_tx, 1, &mut failures);
    let loss_worker_result = loss_worker.join();
    if let Some(observation) = first_report {
        if !observation.engine_mutex_available {
            failures.push("engine mutex was unavailable at the first slow loss report".to_string());
        }
    }
    match loss_worker_result {
        Ok(Ok(ErrorCode::ConnectionLost)) => {}
        Ok(Ok(code)) => failures.push(format!(
            "real workspace resource returned {code:?}, expected ConnectionLost"
        )),
        Ok(Err(error)) => failures.push(error),
        Err(payload) => failures.push(format!("workspace loss worker panicked: {payload:?}")),
    }
    match engine_handle.try_lock() {
        Ok(guard) if guard.is_none() => {}
        Ok(guard) => failures.push(format!(
            "connection loss must clear the engine before reinitialization; present={}",
            guard.is_some()
        )),
        Err(error) => failures.push(format!(
            "engine mutex unavailable after released first report: {error}"
        )),
    }

    // A second public call now takes the post-loss initialization path. The
    // dead endpoint makes Engine::try_daemon_mode emit another slow report.
    // The peer probes `try_lock` synchronously before releasing that response,
    // so this cannot pass merely because a scheduler slept past the 200 ms I/O
    // budget. Current production is red here because ensure_engine holds the
    // engine mutex throughout Engine::new.
    let reinit_server = Arc::clone(&server);
    let reinit_worker = thread::spawn(move || -> Result<ErrorCode, String> {
        match reinit_server.resource_body_for_uri("hyper://workspace") {
            Err(error) => Ok(error.code),
            Ok(value) => Err(format!(
                "dead daemon endpoint unexpectedly reinitialized to resource {value:?}"
            )),
        }
    });
    let second_report =
        receive_and_release_report(&report_seen_rx, &report_release_tx, 2, &mut failures);
    let reinit_worker_result = reinit_worker.join();
    if let Some(observation) = second_report {
        if !observation.engine_mutex_available {
            failures.push(
                "engine mutex was held while post-loss Engine initialization waited on REPORT_HYPERD_ERROR"
                    .to_string(),
            );
        }
    }
    match reinit_worker_result {
        Ok(Ok(ErrorCode::InternalError)) => {}
        Ok(Ok(code)) => failures.push(format!(
            "post-loss initialization returned {code:?}, expected InternalError"
        )),
        Ok(Err(error)) => failures.push(error),
        Err(payload) => failures.push(format!("workspace reinit worker panicked: {payload:?}")),
    }

    let stop_result = health::send_command_with_timeout(
        health_port,
        "STOP",
        Duration::from_secs(1),
        Duration::from_secs(1),
    );
    let peer_result = peer.join();

    match stop_result {
        Ok(response) if response.trim() == "STOPPING" => {}
        Ok(response) => failures.push(format!(
            "controlled health peer returned unexpected STOP response {response:?}"
        )),
        Err(error) => failures.push(format!("could not stop controlled health peer: {error}")),
    }
    match peer_result {
        Ok(Ok(commands))
            if commands
                .iter()
                .filter(|command| command.as_str() == "REPORT_HYPERD_ERROR")
                .count()
                == 2 => {}
        Ok(Ok(commands)) => failures.push(format!(
            "controlled health peer must receive exactly two REPORT_HYPERD_ERROR commands: {commands:?}"
        )),
        Ok(Err(error)) => failures.push(error),
        Err(payload) => failures.push(format!("controlled health peer panicked: {payload:?}")),
    }

    assert!(
        failures.is_empty(),
        "slow health report mutex regression failures:\n{}",
        failures.join("\n")
    );
}

fn receive_and_release_report(
    report_seen_rx: &mpsc::Receiver<ReportObservation>,
    report_release_tx: &mpsc::Sender<usize>,
    expected_sequence: usize,
    failures: &mut Vec<String>,
) -> Option<ReportObservation> {
    let observation = match report_seen_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(observation) => Some(observation),
        Err(error) => {
            failures.push(format!(
                "REPORT_HYPERD_ERROR #{expected_sequence} was not observed: {error}"
            ));
            None
        }
    };
    if let Some(observation) = &observation {
        if observation.sequence != expected_sequence {
            failures.push(format!(
                "observed report sequence {}, expected {expected_sequence}",
                observation.sequence
            ));
        }
        if let Err(error) = report_release_tx.send(observation.sequence) {
            failures.push(format!(
                "could not release report #{} response: {error}",
                observation.sequence
            ));
        }
    }
    observation
}

fn run_controlled_health_peer(
    listener: &TcpListener,
    info: &DaemonInfo,
    engine_probe: &OnceLock<EngineHandle>,
    report_seen_tx: &mpsc::Sender<ReportObservation>,
    report_release_rx: &mpsc::Receiver<usize>,
) -> Result<Vec<String>, String> {
    let mut commands = Vec::new();
    let mut report_sequence = 0_usize;
    loop {
        let (stream, _) = listener
            .accept()
            .map_err(|error| format!("accept controlled health connection: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| format!("bound controlled health read: {error}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| format!("bound controlled health write: {error}"))?;

        let mut reader = BufReader::new(&stream);
        let mut command = String::new();
        reader
            .read_line(&mut command)
            .map_err(|error| format!("read controlled health command: {error}"))?;
        let command = command.trim().to_string();
        commands.push(command.clone());

        let (response, should_stop) = match command.as_str() {
            "PING" => (
                format!(
                    "PONG {} {}\n",
                    health::PONG_TOKEN,
                    hyperdb_mcp::version::MCP_VERSION
                ),
                false,
            ),
            "STATUS" => (
                format!(
                    "{}\n",
                    serde_json::to_string(&info)
                        .map_err(|error| format!("serialize controlled STATUS: {error}"))?
                ),
                false,
            ),
            "HEARTBEAT" => ("OK\n".to_string(), false),
            "REPORT_HYPERD_ERROR" => {
                report_sequence += 1;
                let engine_handle = engine_probe
                    .get()
                    .ok_or_else(|| "engine probe was not installed before report".to_string())?;
                let engine_mutex_available = match engine_handle.try_lock() {
                    Ok(guard) => {
                        drop(guard);
                        true
                    }
                    Err(TryLockError::WouldBlock) => false,
                    Err(TryLockError::Poisoned(_)) => {
                        return Err("engine mutex was poisoned during report probe".to_string());
                    }
                };
                report_seen_tx
                    .send(ReportObservation {
                        sequence: report_sequence,
                        engine_mutex_available,
                    })
                    .map_err(|error| format!("signal observed REPORT_HYPERD_ERROR: {error}"))?;
                let released_sequence = report_release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|error| format!("wait for controlled REPORT release: {error}"))?;
                if released_sequence != report_sequence {
                    return Err(format!(
                        "released report #{released_sequence}, expected #{report_sequence}"
                    ));
                }
                ("OK\n".to_string(), false)
            }
            "STOP" => ("STOPPING\n".to_string(), true),
            other => (format!("ERR unknown command {other}\n"), false),
        };

        (&stream)
            .write_all(response.as_bytes())
            .map_err(|error| format!("write controlled health response: {error}"))?;
        if should_stop {
            return Ok(commands);
        }
    }
}

fn stop_reported_hyperd(pid_path: &Path) -> Result<HyperCleanup, String> {
    let reported = std::fs::read_to_string(pid_path)
        .map_err(|error| format!("Hyper PID was not reported: {error}"))?;
    let pid = reported
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("reported Hyper PID {reported:?} was invalid: {error}"))?;
    if pid == 0 || pid == std::process::id() {
        return Err(format!("refusing unsafe reported Hyper PID {pid}"));
    }

    let mut actively_terminated = false;
    if process_is_alive(pid)? {
        match validate_hyperd_process(pid) {
            Ok(()) => {
                terminate_reported_hyperd(pid)?;
                actively_terminated = true;
            }
            Err(_identity_error) if !process_is_alive(pid)? => {
                // The callback dead-man switch won the race between the first
                // liveness check and process identity inspection. Nothing is
                // left to validate or terminate.
            }
            Err(identity_error) => return Err(identity_error),
        }
    }
    wait_for_process_exit(pid, Duration::from_secs(10))?;
    Ok(HyperCleanup {
        pid,
        actively_terminated,
    })
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> Result<bool, String> {
    let native_pid =
        i32::try_from(pid).map_err(|error| format!("PID does not fit pid_t: {error}"))?;
    // SAFETY: signal 0 does not modify the process; it only checks existence
    // and permission for the exact validated positive PID.
    if unsafe { libc::kill(native_pid, 0) } != 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(format!("poll reported Hyper PID {pid}: {error}")),
        };
    }

    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("inspect state of reported Hyper PID {pid}: {error}"))?;
    if !output.status.success() {
        // The process may have exited between signal-0 and ps. Recheck once
        // before treating an inspection failure as an actual cleanup defect.
        // SAFETY: same exact, validated PID and non-mutating signal 0.
        if unsafe { libc::kill(native_pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return Ok(false);
        }
        return Err(format!("ps could not inspect reported Hyper PID {pid}"));
    }
    let state = String::from_utf8_lossy(&output.stdout);
    // Zombies have terminated and cannot retain the database or consume CPU.
    // They may remain visible until their current parent reaps them, so
    // signal-0 alone is not a valid live-orphan check.
    Ok(!state.trim_start().starts_with('Z'))
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> Result<bool, String> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("run tasklist: {error}"))?;
    if !output.status.success() {
        return Err(format!("tasklist exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")))
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> Result<bool, String> {
    Err("process liveness polling is unsupported on this platform".to_string())
}

#[cfg(unix)]
fn validate_hyperd_process(pid: u32) -> Result<(), String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("inspect reported Hyper PID {pid}: {error}"))?;
    if !output.status.success() {
        return Err(format!("ps could not inspect reported Hyper PID {pid}"));
    }
    let command = String::from_utf8_lossy(&output.stdout);
    let executable = Path::new(command.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if executable != "hyperd" {
        return Err(format!(
            "refusing to terminate reported PID {pid}: process is {command:?}, not hyperd"
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_hyperd_process(pid: u32) -> Result<(), String> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("inspect reported Hyper PID {pid}: {error}"))?;
    if !output.status.success() {
        return Err(format!("tasklist exited with {}", output.status));
    }
    let listing = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if !listing.contains("hyperd.exe") || !listing.contains(&format!("\"{pid}\"")) {
        return Err(format!(
            "refusing to terminate reported PID {pid}: tasklist did not identify hyperd.exe"
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_hyperd_process(_pid: u32) -> Result<(), String> {
    Err("Hyper process identity validation is unsupported on this platform".to_string())
}

#[cfg(unix)]
fn terminate_reported_hyperd(pid: u32) -> Result<(), String> {
    signal_reported_pid(pid, libc::SIGTERM)?;
    if wait_for_process_exit(pid, Duration::from_secs(1)).is_ok() {
        return Ok(());
    }
    signal_reported_pid(pid, libc::SIGKILL)
}

#[cfg(unix)]
fn signal_reported_pid(pid: u32, signal: i32) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|error| format!("PID does not fit pid_t: {error}"))?;
    // SAFETY: the PID was read from the private child report and its executable
    // identity was validated immediately before this call.
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!("signal reported Hyper PID {pid}: {error}"))
    }
}

#[cfg(windows)]
fn terminate_reported_hyperd(pid: u32) -> Result<(), String> {
    let output = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("terminate reported Hyper PID {pid}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "taskkill reported Hyper PID {pid} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_reported_hyperd(_pid: u32) -> Result<(), String> {
    Err("Hyper process termination is unsupported on this platform".to_string())
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_is_alive(pid)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "reported Hyper PID {pid} remained alive after {timeout:?}"
            ));
        }
        // Lifecycle cleanup polling only; behavior synchronization uses the
        // report protocol and channels above.
        thread::sleep(Duration::from_millis(20));
    }
}
