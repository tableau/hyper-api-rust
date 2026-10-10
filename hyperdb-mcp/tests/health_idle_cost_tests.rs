// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reproducible measurement of what an *idle* health listener costs.
//!
//! Issue [#274](https://github.com/tableau/hyper-api-rust/issues/274) replaced
//! the listener's 5 ms accept poll, which cost ~174 context switches per second
//! doing nothing at all. This is the harness behind that number, committed so
//! the claim can be re-derived rather than taken on trust, and so a future
//! change that quietly reinstates a busy poll fails here instead of shipping.
//!
//! Ignored by default: it burns a fixed wall-clock window and reads a
//! process-wide counter, neither of which belongs in the normal suite. Run it
//! deliberately:
//!
//! ```text
//! HYPERD_PATH="$PWD/.hyperd/current" cargo test -p hyperdb-mcp \
//!     --test health_idle_cost_tests --release -- --ignored --nocapture
//! ```
//!
//! `--release` matters — a debug build's loop overhead is not what ships.
//!
//! # Why this file holds exactly one test
//!
//! `getrusage(RUSAGE_SELF)` counts the whole *process*. Cargo gives each
//! integration-test file its own binary, so keeping this alone in one file is
//! what makes the counter attributable to the accept loop: the only other
//! threads alive during the window are the test harness's, parked in a sleep.
//! Adding a second test to this file would silently invalidate the reading.
//!
//! # Reading the counters on macOS
//!
//! `getrusage` reports voluntary (`ru_nvcsw`) and involuntary (`ru_nivcsw`)
//! switches separately, and a thread that parks in a syscall is the textbook
//! *voluntary* case. On Darwin `ru_nvcsw` is not populated and always reads 0,
//! so the sleep/wake cycle lands in `ru_nivcsw` instead. That is a platform
//! artifact, not a measurement error. On Linux `ru_nvcsw` is the meaningful
//! counter. The assertion below sums both so it reads the right thing on either
//! platform, and the printout breaks them out so the Darwin zero is visible
//! rather than mysterious.

#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyperdb_mcp::daemon::control::HealthEndpoint;
use hyperdb_mcp::daemon::discovery::DaemonInfo;
use hyperdb_mcp::daemon::health::{DaemonState, HealthListener};

/// Length of the idle window. Long enough that a per-second wakeup is clearly
/// distinguishable from a 5 ms one (5 versus ~870) without making the run
/// tedious.
const IDLE_WINDOW: Duration = Duration::from_secs(5);

/// Ceiling on context switches across `IDLE_WINDOW`.
///
/// The listener wakes once per second from its readiness timeout, so the
/// expected reading is ~5 plus a couple from the harness itself. The 5 ms poll
/// this replaced produced ~870 over the same window. Anything between the two
/// is a busy poll creeping back; 100 sits far from both, so the test fails on a
/// real regression rather than on a loaded runner.
const MAX_IDLE_CONTEXT_SWITCHES: i64 = 100;

#[derive(Debug, Clone, Copy)]
struct ResourceUsage {
    voluntary_switches: i64,
    involuntary_switches: i64,
    cpu_time: Duration,
}

impl ResourceUsage {
    fn now() -> Self {
        // SAFETY: `usage` is a live, correctly sized `rusage` that `getrusage`
        // only writes into; `RUSAGE_SELF` needs no further resources.
        let usage = unsafe {
            let mut usage = std::mem::zeroed::<libc::rusage>();
            assert_eq!(
                libc::getrusage(libc::RUSAGE_SELF, &raw mut usage),
                0,
                "getrusage(RUSAGE_SELF) failed: {}",
                std::io::Error::last_os_error()
            );
            usage
        };

        Self {
            voluntary_switches: usage.ru_nvcsw,
            involuntary_switches: usage.ru_nivcsw,
            cpu_time: timeval_to_duration(usage.ru_utime) + timeval_to_duration(usage.ru_stime),
        }
    }

    fn since(self, earlier: Self) -> Self {
        Self {
            voluntary_switches: self.voluntary_switches - earlier.voluntary_switches,
            involuntary_switches: self.involuntary_switches - earlier.involuntary_switches,
            cpu_time: self.cpu_time.saturating_sub(earlier.cpu_time),
        }
    }

    fn total_switches(self) -> i64 {
        self.voluntary_switches + self.involuntary_switches
    }
}

/// `tv_sec` is `time_t` and `tv_usec` is `suseconds_t` — 64-bit on Linux,
/// 32-bit on macOS — so both are converted fallibly rather than with an `as`
/// cast that would differ in behaviour per platform. Neither can be negative
/// for an elapsed CPU time; a nonsensical value degrades to zero rather than
/// wrapping into an enormous duration.
fn timeval_to_duration(value: libc::timeval) -> Duration {
    let seconds = u64::try_from(value.tv_sec).unwrap_or(0);
    let microseconds = u64::try_from(value.tv_usec).unwrap_or(0);
    Duration::from_secs(seconds) + Duration::from_micros(microseconds)
}

fn idle_daemon_info(endpoint: &HealthEndpoint) -> DaemonInfo {
    DaemonInfo {
        pid: std::process::id(),
        hyperd_endpoint: "127.0.0.1:0".to_string(),
        health_endpoint: endpoint.as_str().to_string(),
        started_at: "2026-09-06T00:00:00Z".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Binds a real listener on a socket inside a private temporary directory,
/// leaves it completely alone for `IDLE_WINDOW`, and reports what the process
/// paid for the privilege.
///
/// No client ever connects and no discovery record is written — the socket
/// lives in a throwaway directory, never the real state directory — so this
/// cannot disturb a daemon running on the same machine.
#[test]
#[ignore = "burns a fixed 5s wall-clock window and reads a process-wide counter; run deliberately"]
fn idle_health_listener_costs_about_one_wakeup_per_second() {
    let state_dir = tempfile::tempdir().expect("create idle-cost state dir");
    let endpoint =
        HealthEndpoint::for_new_daemon(state_dir.path()).expect("derive idle-cost health endpoint");
    let listener = HealthListener::bind(&endpoint).expect("bind idle-cost health listener");
    let state = Arc::new(DaemonState::new(endpoint.clone()));
    let info = Arc::new(Mutex::new(idle_daemon_info(&endpoint)));

    let run_state = Arc::clone(&state);
    let server = std::thread::spawn(move || listener.run(run_state, info));

    // Let the loop reach its first readiness wait before the counter is read,
    // so thread startup is not charged to the idle window.
    std::thread::sleep(Duration::from_millis(200));

    let before = ResourceUsage::now();
    let started_at = Instant::now();
    std::thread::sleep(IDLE_WINDOW);
    let observed_window = started_at.elapsed();
    let idle = ResourceUsage::now().since(before);

    let shutdown_at = Instant::now();
    state.request_shutdown();
    server.join().expect("idle-cost listener must not panic");
    let shutdown_latency = shutdown_at.elapsed();

    let per_second = f64::from(i32::try_from(idle.total_switches()).unwrap_or(i32::MAX))
        / observed_window.as_secs_f64();
    println!(
        "idle window {observed_window:?}\n  \
         context switches: {} total ({} voluntary, {} involuntary) = {per_second:.1}/s\n  \
         cpu time:         {:?}\n  \
         shutdown latency: {shutdown_latency:?}",
        idle.total_switches(),
        idle.voluntary_switches,
        idle.involuntary_switches,
        idle.cpu_time,
    );

    assert!(
        idle.total_switches() <= MAX_IDLE_CONTEXT_SWITCHES,
        "an idle health listener burned {} context switches over {observed_window:?} \
         ({per_second:.1}/s), past the {MAX_IDLE_CONTEXT_SWITCHES} ceiling — the accept loop \
         is polling on a timer again instead of waiting on the socket",
        idle.total_switches()
    );
}
