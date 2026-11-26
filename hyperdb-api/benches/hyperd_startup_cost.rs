// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `hyperd` startup-cost and idle-memory measurement.
//!
//! Measures, over N iterations (default 30, first argv):
//!
//! 1. Per-phase wall time of `HyperProcess::new` (spawn to endpoint),
//!    `Connection` connect, the first query (`SELECT 1`) and graceful
//!    shutdown, each as min / median / p95.
//! 2. The resident set size of one idle `hyperd` after 5 seconds
//!    (`ps -o rss=` on Unix; skipped on Windows).
//! 3. Connect time against an engine that requires SCRAM-SHA-256
//!    authentication, next to the password-less connect time, so the
//!    per-connection SCRAM cost is visible.
//!
//! Run with:
//! ```sh
//! HYPERD_PATH="$PWD/.hyperd/current" \
//!   cargo run -p hyperdb-api --release --example hyperd_startup_cost -- 30
//! # Unix Domain Socket / Named Pipe instead of TCP:
//! BENCH_TRANSPORT=ipc HYPERD_PATH="$PWD/.hyperd/current" \
//!   cargo run -p hyperdb-api --release --example hyperd_startup_cost -- 30
//! ```
//!
//! The SCRAM leg always uses TCP: password authentication is only
//! meaningful on a network transport.

// Benchmark harness: KiB-to-MiB display conversion cannot meaningfully lose
// precision at these magnitudes.
#![allow(clippy::cast_precision_loss, reason = "benchmark display conversion")]

use hyperdb_api::{ConnectionBuilder, HyperProcess, Parameters, Result, TransportMode};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Benchmark-only fixed test credential. The config below declares one
/// superuser whose SCRAM-SHA-256 verifier matches `BENCH_PASSWORD`. It
/// protects nothing: the engine it configures lives for seconds, listens
/// on loopback, and holds no data.
const BENCH_USER: &str = "tableau_internal_user";
const BENCH_PASSWORD: &str = "probe-generated-secret-123";
const BENCH_CONFIG: &str = r#"{"authorized_users": [{"name": "tableau_internal_user", "superuser": true, "authentication": {"method": "scram_sha256", "encrypted_password": "SCRAM-SHA-256$4096:hj0USDEqrBHS7KZr8zc/3A==$VlNk9UZfx1YDt9eq/oc2zpoeozdLv3YQokH+bIxrPsQ=:mteEFio2UZMs7qwsfRdsAr7EItnZnSh31XoIwdjJcrM="}}]}"#;

/// min / median / p95 of a set of samples, in milliseconds.
struct Stats {
    min: f64,
    median: f64,
    p95: f64,
}

fn stats(samples: &[Duration]) -> Stats {
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(f64::total_cmp);
    let n = ms.len();
    let median = if n % 2 == 1 {
        ms[n / 2]
    } else {
        f64::midpoint(ms[n / 2 - 1], ms[n / 2])
    };
    // Nearest-rank percentile: ceil(0.95 * n) - 1.
    let p95_idx = (n * 95).div_ceil(100).saturating_sub(1);
    Stats {
        min: ms[0],
        median,
        p95: ms[p95_idx],
    }
}

fn row(label: &str, samples: &[Duration]) {
    let s = stats(samples);
    println!(
        "  {label:<28} {:>10.2} {:>10.2} {:>10.2}",
        s.min, s.median, s.p95
    );
}

fn params(transport: TransportMode, log_dir: &Path) -> Parameters {
    let mut p = Parameters::new();
    p.set("log_dir", log_dir.to_string_lossy().into_owned());
    p.set_transport_mode(transport);
    p
}

fn endpoint_of(hyper: &HyperProcess) -> Result<String> {
    hyper
        .connection_endpoint_string()
        .ok_or_else(|| hyperdb_api::Error::internal("HyperProcess has no endpoint"))
}

/// Resident set size of `pid` in KiB via `ps`, or `None` off Unix.
#[cfg(unix)]
fn rss_kib(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(not(unix))]
fn rss_kib(_pid: u32) -> Option<u64> {
    None
}

fn main() -> Result<()> {
    let iterations: usize = env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(30);

    // Same switch as `benchmark_suite`: BENCH_TRANSPORT=ipc selects the
    // Unix Domain Socket / Named Pipe transport; default is TCP.
    let transport = match env::var("BENCH_TRANSPORT").ok().as_deref() {
        Some("ipc" | "IPC" | "pipe") => TransportMode::Ipc,
        _ => TransportMode::Tcp,
    };

    let work_dir: PathBuf =
        env::temp_dir().join(format!("hyperd_startup_cost_{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).map_err(|e| hyperdb_api::Error::internal(e.to_string()))?;

    println!("hyperd startup cost");
    println!("  Transport:  {transport:?}");
    println!("  Iterations: {iterations}");
    println!();

    // ---- Phase timings: spawn -> connect -> SELECT 1 -> shutdown ----
    let mut spawn_t = Vec::with_capacity(iterations);
    let mut connect_t = Vec::with_capacity(iterations);
    let mut query_t = Vec::with_capacity(iterations);
    let mut shutdown_t = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let p = params(transport, &work_dir);
        let t = Instant::now();
        let hyper = HyperProcess::new(None, Some(&p))?;
        spawn_t.push(t.elapsed());
        let endpoint = endpoint_of(&hyper)?;

        let t = Instant::now();
        let conn = ConnectionBuilder::new(&endpoint).build()?;
        connect_t.push(t.elapsed());

        let t = Instant::now();
        let one: i32 = conn.fetch_scalar("SELECT 1")?;
        query_t.push(t.elapsed());
        assert_eq!(one, 1);

        conn.close()?;
        let t = Instant::now();
        hyper.shutdown_graceful()?;
        shutdown_t.push(t.elapsed());
    }

    println!("Per-phase wall time (ms), {iterations} iterations");
    println!(
        "  {:<28} {:>10} {:>10} {:>10}",
        "phase", "min", "median", "p95"
    );
    row("spawn to endpoint", &spawn_t);
    row("connect (no password)", &connect_t);
    row("first query (SELECT 1)", &query_t);
    row("shutdown", &shutdown_t);
    println!();

    // ---- Idle resident memory ----
    {
        let p = params(transport, &work_dir);
        let hyper = HyperProcess::new(None, Some(&p))?;
        std::thread::sleep(Duration::from_secs(5));
        match hyper.pid().and_then(|pid| rss_kib(pid).map(|k| (pid, k))) {
            Some((pid, kib)) => println!(
                "Idle hyperd after 5 s: pid {pid}, RSS {kib} KiB ({:.1} MiB)",
                kib as f64 / 1024.0
            ),
            None if cfg!(unix) => println!("Idle hyperd RSS: could not read via ps"),
            None => println!("Idle hyperd RSS: skipped on Windows (no ps; use tasklist manually)"),
        }
        hyper.shutdown_graceful()?;
        println!();
    }

    // ---- Connect cost: password-less vs SCRAM-SHA-256 (TCP) ----
    let config_path = work_dir.join("scram-config.json");
    std::fs::write(&config_path, BENCH_CONFIG)
        .map_err(|e| hyperdb_api::Error::internal(e.to_string()))?;

    let plain_params = params(TransportMode::Tcp, &work_dir);
    let plain = HyperProcess::new(None, Some(&plain_params))?;
    let plain_ep = endpoint_of(&plain)?;

    let mut scram_params = params(TransportMode::Tcp, &work_dir);
    scram_params.set("config", config_path.to_string_lossy().into_owned());
    scram_params.set("strict_settings_mode", "");
    let scram = HyperProcess::new(None, Some(&scram_params))?;
    let scram_ep = endpoint_of(&scram)?;

    let mut plain_t = Vec::with_capacity(iterations);
    let mut scram_t = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let t = Instant::now();
        let c = ConnectionBuilder::new(&plain_ep).build()?;
        plain_t.push(t.elapsed());
        c.close()?;

        let t = Instant::now();
        let c = ConnectionBuilder::new(&scram_ep)
            .user(BENCH_USER)
            .password(BENCH_PASSWORD)
            .build()?;
        scram_t.push(t.elapsed());
        c.close()?;
    }
    plain.shutdown_graceful()?;
    scram.shutdown_graceful()?;

    println!("Connect time (ms), TCP, {iterations} iterations on a warm engine");
    println!(
        "  {:<28} {:>10} {:>10} {:>10}",
        "auth", "min", "median", "p95"
    );
    row("password-less", &plain_t);
    row("SCRAM-SHA-256", &scram_t);
    let delta = stats(&scram_t).median - stats(&plain_t).median;
    println!("  SCRAM median overhead: {delta:.2} ms");

    std::fs::remove_dir_all(&work_dir).ok();
    Ok(())
}
