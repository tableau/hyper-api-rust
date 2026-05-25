// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for `HyperProcess` lifecycle helpers — specifically the `has_exited`
//! probe used by the hyperdb-mcp daemon's restart logic.

mod common;

use common::test_hyper_params;
use hyperdb_api::HyperProcess;

/// A freshly-spawned hyperd should not appear exited.
#[test]
fn has_exited_returns_false_for_running_hyperd() {
    let params = test_hyper_params("has_exited_running").unwrap();
    let mut hyper = HyperProcess::new(None, Some(&params)).unwrap();
    assert!(
        !hyper.has_exited(),
        "freshly-spawned hyperd should be running"
    );
}

/// After SIGKILL, `has_exited` must observe the child as exited.
/// This is the path the daemon's monitor relies on to detect a dead hyperd.
/// Reaping the child as a side effect is also exercised here — without it,
/// the next `has_exited` call could return false on a zombie.
#[cfg(unix)]
#[test]
fn has_exited_returns_true_after_sigkill() {
    use std::process::Command;
    use std::time::Duration;

    let params = test_hyper_params("has_exited_killed").unwrap();
    let mut hyper = HyperProcess::new(None, Some(&params)).unwrap();
    let pid = hyper.pid().expect("hyperd should have a pid");

    // Kill the process directly. The HyperProcess `Drop` would also kill it,
    // but we need to test detection while we still own the handle.
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill -9 should succeed");
    assert!(status.success(), "kill -9 returned non-zero");

    // Give the OS a moment to reap the SIGKILL'd process and update its state.
    // Up to 2 seconds in 50ms increments — under load CI may take longer than
    // a tight loop expects, but normal latency is sub-100ms.
    let mut detected = false;
    for _ in 0..40 {
        if hyper.has_exited() {
            detected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        detected,
        "has_exited should observe the killed child within 2s"
    );
}
