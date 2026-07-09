// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Key-value store write benchmark.
//!
//! Compares two write strategies against a real `hyperd`:
//! - single-commit-per-set: one `KvStore::set` per key (implicit commit)
//! - batched: `KvStore::set_batch` of `BATCH` keys per transaction
//!
//! Run with:
//!   cargo run -p hyperdb-api --release --example kv_benchmark            # default 50k keys
//!   cargo run -p hyperdb-api --release --example kv_benchmark 200000     # 200k keys

// Throughput math converts a `usize` key count to `f64`; the resulting
// precision loss is irrelevant for a keys/sec figure. `allow` (not `expect`)
// because this is the only pedantic cast lint that fires here — an `expect`
// listing others would trip `unfulfilled_lint_expectations` under `-D warnings`.
#![allow(
    clippy::cast_precision_loss,
    reason = "benchmark throughput math needs usize -> f64; precision loss is cosmetic"
)]

use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};
use std::env;
use std::time::Instant;

const DEFAULT_KEYS: usize = 50_000;
const BATCH: usize = 25; // within the requested 10-50 range

fn key_count() -> usize {
    env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_KEYS)
}

fn throughput(label: &str, keys: usize, secs: f64) {
    let per_sec = if secs > 0.0 { keys as f64 / secs } else { 0.0 };
    println!("  {label:<28} {keys} keys in {secs:>7.3}s  =>  {per_sec:>12.0} keys/sec");
}

fn bench_single(conn: &Connection, keys: usize) -> Result<f64> {
    let kv = conn.kv_store("bench_single")?;
    kv.clear()?;
    let start = Instant::now();
    for i in 0..keys {
        kv.set(&format!("k{i}"), "value")?;
    }
    Ok(start.elapsed().as_secs_f64())
}

fn bench_batched(conn: &Connection, keys: usize) -> Result<f64> {
    let kv = conn.kv_store("bench_batched")?;
    kv.clear()?;
    let start = Instant::now();
    let mut i = 0;
    while i < keys {
        let end = (i + BATCH).min(keys);
        // Own the strings, then borrow into the &[(&str, &str)] slice.
        let owned: Vec<(String, String)> = (i..end)
            .map(|n| (format!("k{n}"), "value".to_string()))
            .collect();
        let batch: Vec<(&str, &str)> = owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        kv.set_batch(&batch)?;
        i = end;
    }
    Ok(start.elapsed().as_secs_f64())
}

fn main() -> Result<()> {
    let keys = key_count();
    println!("\n=== KV Store write benchmark ({keys} keys, batch size {BATCH}) ===");

    let db_path = std::env::temp_dir().join("kv_benchmark.hyper");
    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, &db_path, CreateMode::CreateAndReplace)?;

    let single_secs = bench_single(&conn, keys)?;
    throughput("single commit per set", keys, single_secs);

    let batched_secs = bench_batched(&conn, keys)?;
    throughput(&format!("batched ({BATCH}/txn)"), keys, batched_secs);

    if batched_secs > 0.0 {
        println!(
            "\n  speedup (batched vs single): {:.2}x",
            single_secs / batched_secs
        );
    }
    Ok(())
}
