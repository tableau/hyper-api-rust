// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for `HyperProcess` instance management.
//!
//! Includes tests for the callback connection "dead man's switch" mechanism
//! that ensures Hyper shuts down gracefully when the client process exits.

mod common;

use hyperdb_api::{Connection, CreateMode, HyperProcess};
use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;

const CALLBACK_PARENT_KILL_CHILD_ENV: &str = "HYPERDB_CALLBACK_PARENT_KILL_CHILD";

/// Killing the owning client process must close its callback connection, so
/// `hyperd` shuts itself down even though the client's `Drop` implementation
/// never gets a chance to run.
#[test]
fn callback_connection_shutdowns_hyperd_after_parent_kill() {
    if let Some(pid_file) = std::env::var_os(CALLBACK_PARENT_KILL_CHILD_ENV) {
        let params = common::test_hyper_params("callback_connection_parent_kill_child")
            .expect("child must create Hyper parameters");
        let hyper = HyperProcess::new(None, Some(&params)).expect("child must start HyperProcess");
        let pid = hyper
            .pid()
            .expect("child must report HyperProcess public PID");
        fs::write(pid_file, pid.to_string()).expect("child must report Hyper PID to parent");

        loop {
            thread::park();
        }
    }

    let temp_dir = tempfile::tempdir().expect("parent must create RAII temp directory");
    let pid_file = temp_dir.path().join("hyperd-pid");
    let test_name = "callback_connection_shutdowns_hyperd_after_parent_kill";
    let mut child = Command::new(std::env::current_exe().expect("test executable path"))
        .args(["--exact", test_name, "--nocapture"])
        .env(CALLBACK_PARENT_KILL_CHILD_ENV, &pid_file)
        .spawn()
        .expect("parent must start exact helper child");

    let pid = wait_for_reported_pid(&pid_file, Duration::from_secs(10)).unwrap_or_else(|message| {
        let _ = child.kill();
        let _ = child.wait();
        panic!("callback helper failed before reporting hyperd PID: {message}");
    });
    assert!(
        is_process_running(pid),
        "reported hyperd PID {pid} must be live before its parent is killed"
    );

    child.kill().expect("parent must kill exact helper child");
    let child_status = child
        .wait()
        .expect("parent must wait for killed helper child");
    assert!(
        !child_status.success(),
        "the deliberately killed helper child must not report success"
    );

    let shutdown_detected = bounded_process_exit_poll(pid, Duration::from_secs(10));
    assert!(
        shutdown_detected,
        "hyperd PID {pid} remained live after its callback-owning parent was killed"
    );
}

fn wait_for_reported_pid(pid_file: &std::path::Path, timeout: Duration) -> Result<u32, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match fs::read_to_string(pid_file) {
            Ok(contents) => {
                return contents
                    .trim()
                    .parse()
                    .map_err(|error| format!("invalid PID report {contents:?}: {error}"));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not read PID report: {error}")),
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("no PID report appeared at {}", pid_file.display()));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn bounded_process_exit_poll(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_process_running(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn test_hyper_process_start_stop() {
    let params = common::test_hyper_params("test_hyper_process_start_stop")
        .expect("Failed to create test parameters");
    let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start Hyper process");

    // Verify the endpoint is valid
    let endpoint = hyper.endpoint().expect("No endpoint");
    let descriptor = endpoint.to_string();
    assert!(!descriptor.is_empty());

    // Verify we can connect
    let conn = Connection::without_database(endpoint).expect("Failed to connect");
    let mut rowset = conn
        .execute_query("SELECT 17")
        .expect("Failed to execute query");
    let chunk = rowset
        .next_chunk()
        .expect("Failed to get chunk")
        .expect("Expected chunk");
    let row = chunk.first().expect("Expected row");
    let result = row.get_i32(0).expect("NULL value");
    assert_eq!(result, 17);

    // Process will be shut down when dropped
}

#[test]
fn test_hyper_process_multiple_instances() {
    let params1 = common::test_hyper_params("test_hyper_process_multiple_instances_1")
        .expect("Failed to create test parameters");
    let params2 = common::test_hyper_params("test_hyper_process_multiple_instances_2")
        .expect("Failed to create test parameters");
    let hyper1 =
        HyperProcess::new(None, Some(&params1)).expect("Failed to start first Hyper process");
    let hyper2 =
        HyperProcess::new(None, Some(&params2)).expect("Failed to start second Hyper process");

    // Both should be able to connect
    let conn1 = Connection::without_database(hyper1.endpoint().unwrap())
        .expect("Failed to connect to first instance");
    let conn2 = Connection::without_database(hyper2.endpoint().unwrap())
        .expect("Failed to connect to second instance");

    let mut rowset1 = conn1
        .execute_query("SELECT 42")
        .expect("Failed to execute query");
    let chunk1 = rowset1
        .next_chunk()
        .expect("Failed to get chunk")
        .expect("Expected chunk");
    let result1 = chunk1
        .first()
        .expect("Expected row")
        .get_i32(0)
        .expect("NULL value");

    let mut rowset2 = conn2
        .execute_query("SELECT 99")
        .expect("Failed to execute query");
    let chunk2 = rowset2
        .next_chunk()
        .expect("Failed to get chunk")
        .expect("Expected chunk");
    let result2 = chunk2
        .first()
        .expect("Expected row")
        .get_i32(0)
        .expect("NULL value");

    assert_eq!(result1, 42);
    assert_eq!(result2, 99);
}

#[test]
fn test_hyper_process_endpoint_descriptor() {
    let params = common::test_hyper_params("test_hyper_process_endpoint_descriptor")
        .expect("Failed to create test parameters");
    let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start Hyper process");

    let endpoint = hyper.endpoint().expect("No endpoint");
    let descriptor = endpoint.to_string();

    // Should have host and port format
    assert!(
        descriptor.contains(':'),
        "Endpoint should be host:port format"
    );
}

#[test]
fn test_hyper_process_connection_new() {
    let params = common::test_hyper_params("test_hyper_process_connection_new")
        .expect("Failed to create test parameters");
    let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start Hyper process");

    let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("test.hyper");

    // Use Connection::new() which takes HyperProcess
    let conn =
        Connection::new(&hyper, &db_path, CreateMode::CreateAndReplace).expect("Failed to connect");

    conn.execute_command("CREATE TABLE test (id INT)")
        .expect("Failed to create table");

    let mut result = conn.execute_query("SELECT 123").expect("Failed to query");
    let chunk = result
        .next_chunk()
        .expect("Failed to get chunk")
        .expect("Expected chunk");
    let value = chunk
        .first()
        .expect("Expected row")
        .get_i32(0)
        .expect("NULL value");
    assert_eq!(value, 123);
}

#[test]
fn test_hyper_process_telemetry() {
    // Test with telemetry disabled (the only option we support for testing)
    let params = common::test_hyper_params("test_hyper_process_telemetry")
        .expect("Failed to create test parameters");
    let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start Hyper process");

    // Just verify process starts correctly with telemetry disabled
    let _conn = Connection::without_database(hyper.endpoint().unwrap()).expect("Failed to connect");
}

#[test]
fn test_hyper_process_drop() {
    // Test that drop properly cleans up
    let endpoint_str;
    {
        let params = common::test_hyper_params("test_hyper_process_drop")
            .expect("Failed to create test parameters");
        let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start Hyper process");
        endpoint_str = hyper.endpoint().unwrap().to_string();

        // Connection should work
        let conn =
            Connection::without_database(hyper.endpoint().unwrap()).expect("Failed to connect");
        conn.execute_query("SELECT 1").expect("Failed to query");
    }
    // After hyper is dropped, we can't verify the server is down without
    // trying to connect (which would hang), so we just ensure no panic
    assert!(!endpoint_str.is_empty());
}

#[test]
fn test_hyper_process_create_multiple_databases() {
    let params = common::test_hyper_params("test_hyper_process_create_multiple_databases")
        .expect("Failed to create test parameters");
    let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start Hyper process");

    let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
    let db1_path = temp_dir.path().join("db1.hyper");
    let db2_path = temp_dir.path().join("db2.hyper");

    // Create two separate databases
    {
        let conn1 = Connection::new(&hyper, &db1_path, CreateMode::CreateAndReplace)
            .expect("Failed to create db1");
        conn1
            .execute_command("CREATE TABLE t1 (id INT)")
            .expect("Failed to create table");
    }

    {
        let conn2 = Connection::new(&hyper, &db2_path, CreateMode::CreateAndReplace)
            .expect("Failed to create db2");
        conn2
            .execute_command("CREATE TABLE t2 (id INT)")
            .expect("Failed to create table");
    }

    // Verify both databases exist and have their tables
    {
        let conn1 = Connection::new(&hyper, &db1_path, CreateMode::DoNotCreate)
            .expect("Failed to open db1");
        assert!(conn1.execute_query("SELECT * FROM t1").is_ok());
    }

    {
        let conn2 = Connection::new(&hyper, &db2_path, CreateMode::DoNotCreate)
            .expect("Failed to open db2");
        assert!(conn2.execute_query("SELECT * FROM t2").is_ok());
    }
}

/// Test that `HyperProcess` gracefully shuts down via the callback connection.
///
/// This tests the "dead man's switch" mechanism:
/// 1. Start `HyperProcess`
/// 2. Get the PID
/// 3. Drop the `HyperProcess` (closes callback connection)
/// 4. Verify the process exits gracefully (not running anymore)
#[test]
fn test_callback_connection_graceful_shutdown() {
    let pid;
    {
        let params = common::test_hyper_params("test_callback_connection_graceful_shutdown")
            .expect("Failed to create test parameters");
        let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start Hyper process");

        pid = hyper.pid().expect("Should have PID");

        // Verify process is running
        assert!(
            is_process_running(pid),
            "Process should be running after start"
        );

        // Verify we can connect and query
        let conn =
            Connection::without_database(hyper.endpoint().unwrap()).expect("Failed to connect");
        conn.execute_query("SELECT 1").expect("Query should work");

        // HyperProcess will be dropped here, closing the callback connection
    }

    // Give Hyper time to shut down gracefully (should be quick with callback)
    let mut shutdown_detected = false;
    for _ in 0..50 {
        // Max 5 seconds
        thread::sleep(Duration::from_millis(100));
        if !is_process_running(pid) {
            shutdown_detected = true;
            break;
        }
    }

    assert!(
        shutdown_detected,
        "Hyper process (pid={pid}) should have shut down after callback connection closed"
    );
}

/// Test that explicit `shutdown_timeout` works correctly.
#[test]
fn test_shutdown_timeout() {
    let params = common::test_hyper_params("test_shutdown_timeout")
        .expect("Failed to create test parameters");
    let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start Hyper process");

    let pid = hyper.pid().expect("Should have PID");
    assert!(is_process_running(pid), "Process should be running");

    // Verify connectivity before shutdown
    let conn = Connection::without_database(hyper.endpoint().unwrap()).expect("Failed to connect");
    conn.execute_query("SELECT 42").expect("Query should work");
    drop(conn); // Close connection before shutdown

    // Explicit shutdown with timeout
    hyper
        .shutdown_timeout(Duration::from_secs(5))
        .expect("Shutdown should succeed");

    // Process should be gone
    assert!(
        !is_process_running(pid),
        "Process should not be running after shutdown"
    );
}

/// Test that the callback connection mechanism reports correct endpoint.
#[test]
fn test_callback_endpoint_format() {
    let params = common::test_hyper_params("test_callback_endpoint_format")
        .expect("Failed to create test parameters");
    let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start Hyper process");

    let endpoint = hyper.endpoint().expect("No endpoint");

    // Endpoint should be in host:port format (from callback connection)
    assert!(endpoint.contains(':'), "Endpoint should contain ':'");

    // Should be parseable as host:port
    // Use rfind to handle IPv6 addresses like [::1]:port
    let colon_idx = endpoint.rfind(':').expect("No colon in endpoint");
    let host = &endpoint[..colon_idx];
    let port_str = &endpoint[colon_idx + 1..];

    // Port should be a valid number
    let port: u16 = port_str
        .parse()
        .unwrap_or_else(|e| panic!("Port '{port_str}' should be a valid number: {e:?}"));
    assert!(port > 0, "Port should be positive");

    // Host should be localhost or 127.0.0.1
    assert!(
        host == "localhost" || host == "127.0.0.1",
        "Host should be localhost or 127.0.0.1, got: {host}"
    );
}

/// Helper function to check if a process is running.
fn is_process_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[cfg(windows)]
    {
        // On Windows, use tasklist to check if process exists
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}")])
            .output()
            .is_ok_and(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains(&pid.to_string())
            })
    }
}
