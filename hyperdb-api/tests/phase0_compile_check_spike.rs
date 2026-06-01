// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Phase 0 gating spikes for the compile-time SQL validator
//! (see work-notes COMPILE_TIME_SQL_VALIDATOR_PLAN.md).
//!
//! These are throwaway measurement harnesses, NOT permanent tests. They
//! answer the empirical questions that gate the whole project:
//!
//!   * startup timing  — resolves the "20ms vs 10s" conflict for the
//!     "spin up Hyper inside the proc-macro host" architecture.
//!   * concurrency     — S3: N concurrent `HyperProcess::new()` in one
//!     process (mimics `cargo build -j N` proc-macro hosts) — collisions?
//!   * error shape     — can we extract the missing-table name from
//!     Hyper's diagnostic? (the "Hyper-first table extraction" approach
//!     proposed in discussion #90, replacing the sqlparser pre-parse).
//!   * LIMIT 0 dry-run — does a zero-row query still return a populated
//!     `ResultSchema` with column names + `SqlType`s?
//!
//! Run with output shown:
//!   HYPERD_PATH set (or .hyperd/current present), then:
//!   cargo test -p hyperdb-api --test phase0_compile_check_spike -- --nocapture --test-threads=1

use std::time::Instant;

use hyperdb_api::{Connection, CreateMode, HyperProcess, Parameters};

mod common;

/// Build params that log into `test_results` (keeps the repo tidy).
fn params() -> Parameters {
    let dir = std::env::current_dir().unwrap().join("test_results");
    std::fs::create_dir_all(&dir).unwrap();
    let mut p = Parameters::new();
    p.set(
        "log_dir",
        dir.canonicalize().unwrap().to_string_lossy().to_string(),
    );
    p
}

fn temp_db(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(format!("{tag}.hyper"));
    (dir, db)
}

// ---------------------------------------------------------------------------
// Spike 1: startup timing
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Phase 0 measurement spike; run manually with --ignored --nocapture"]
fn spike_startup_timing() {
    // Cold start: first process in this test binary.
    let t0 = Instant::now();
    let hyper = HyperProcess::new(None, Some(&params())).expect("start hyperd");
    let cold = t0.elapsed();

    // First connection (creates a fresh .hyper db).
    let (_g, db) = temp_db("spike_timing");
    let t1 = Instant::now();
    let conn = Connection::new(&hyper, &db, CreateMode::CreateAndReplace).expect("connect");
    let connect = t1.elapsed();

    // A trivial query to time the round-trip once warm.
    let t2 = Instant::now();
    let _ = conn.execute_query("SELECT 1").expect("select 1");
    let first_query = t2.elapsed();

    // Subsequent warm process starts within the SAME binary — this is the
    // number that matters for "shared instance per crate compilation": once
    // one instance is up, how cheap is reusing it? (We reuse `hyper`, but
    // also measure a *second* fresh process to bound worst-case per-host.)
    let t3 = Instant::now();
    let hyper2 = HyperProcess::new(None, Some(&params())).expect("start hyperd #2");
    let warm_second_process = t3.elapsed();
    drop(hyper2);

    println!("\n=== SPIKE 1: startup timing ===");
    println!("cold HyperProcess::new()      : {cold:?}");
    println!("Connection::new() (create db) : {connect:?}");
    println!("first 'SELECT 1' round-trip   : {first_query:?}");
    println!("2nd HyperProcess::new()       : {warm_second_process:?}");
    println!("(architecture amortizes ONE start across all macros in a crate)\n");
}

// ---------------------------------------------------------------------------
// Spike 2 (S3): concurrency — N processes at once in one OS process
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Phase 0 measurement spike; run manually with --ignored --nocapture"]
fn spike_concurrency_stress() {
    const N: usize = 16;
    println!("\n=== SPIKE 2 (S3): {N} concurrent HyperProcess::new() ===");

    let t0 = Instant::now();
    let handles: Vec<_> = (0..N)
        .map(|i| {
            std::thread::spawn(move || {
                let start = Instant::now();
                let hyper = HyperProcess::new(None, Some(&params()))
                    .unwrap_or_else(|e| panic!("instance {i} failed to start: {e}"));
                let (_g, db) = temp_db(&format!("spike_conc_{i}"));
                let conn = Connection::new(&hyper, &db, CreateMode::CreateAndReplace)
                    .unwrap_or_else(|e| panic!("instance {i} failed to connect: {e}"));
                let v: i32 = conn
                    .execute_scalar_query::<i32>("SELECT 1")
                    .expect("scalar")
                    .expect("non-null");
                assert_eq!(v, 1, "instance {i} returned wrong value");
                (i, start.elapsed())
            })
        })
        .collect();

    let mut elapsed = Vec::new();
    let mut failures = 0;
    for h in handles {
        match h.join() {
            Ok((i, dur)) => elapsed.push((i, dur)),
            Err(_) => failures += 1,
        }
    }
    let wall = t0.elapsed();

    elapsed.sort_by_key(|(_, d)| *d);
    let slowest = elapsed.last().map(|(_, d)| *d).unwrap_or_default();
    let fastest = elapsed.first().map(|(_, d)| *d).unwrap_or_default();

    println!(
        "started+queried {} / {N} instances concurrently",
        elapsed.len()
    );
    println!("failures: {failures}");
    println!("per-instance fastest: {fastest:?}, slowest: {slowest:?}");
    println!("total wall-clock for all {N}: {wall:?}");
    println!("(if no collisions/port conflicts, all {N} succeed)\n");

    assert_eq!(
        failures, 0,
        "some concurrent instances failed — collision risk!"
    );
}

// ---------------------------------------------------------------------------
// Spike 3: missing-table error shape (Hyper-first extraction)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Phase 0 measurement spike; run manually with --ignored --nocapture"]
fn spike_missing_table_error_shape() {
    let hyper = HyperProcess::new(None, Some(&params())).expect("start hyperd");
    let (_g, db) = temp_db("spike_err");
    let conn = Connection::new(&hyper, &db, CreateMode::CreateAndReplace).expect("connect");

    println!("\n=== SPIKE 3: missing-table error shape ===");

    // NOTE: execute_query is LAZY on the TCP transport — the query isn't
    // actually run (and errors don't arrive) until next_chunk() pulls
    // bytes. So a dry-run helper MUST drive the stream, not just call
    // execute_query and check is_err(). This closure mimics what the
    // real dry-run would do: run + drain to first chunk.
    let dry_run = |sql: &str| -> hyperdb_api::Result<()> {
        let mut rs = conn.execute_query(sql)?;
        // Drain — the first next_chunk() is where a server ErrorResponse
        // surfaces (or where RowDescription/schema arrives on success).
        while rs.next_chunk()?.is_some() {}
        Ok(())
    };

    // Query a table that doesn't exist.
    let err = dry_run("SELECT * FROM ghosts").expect_err("should fail: table doesn't exist");
    let msg = format!("{err}");
    let dbg = format!("{err:?}");
    println!("Display : {msg}");
    println!("Debug   : {dbg}");
    println!("contains 'ghosts': {}", msg.contains("ghosts"));

    // Missing COLUMN on an existing table.
    conn.execute_command("CREATE TABLE t (id BIGINT, name TEXT)")
        .expect("create t");
    let col_err =
        dry_run("SELECT id, ema1l FROM t").expect_err("should fail: column doesn't exist");
    let col_msg = format!("{col_err}");
    println!("\nmissing-column Display: {col_msg}");
    println!("contains 'ema1l': {}", col_msg.contains("ema1l"));
    println!("(if the bad identifier appears verbatim, Hyper-first extraction is viable)\n");
}

// ---------------------------------------------------------------------------
// Spike 4: LIMIT 0 dry-run returns a populated schema
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Phase 0 measurement spike; run manually with --ignored --nocapture"]
fn spike_limit_zero_dry_run() {
    let hyper = HyperProcess::new(None, Some(&params())).expect("start hyperd");
    let (_g, db) = temp_db("spike_dry");
    let conn = Connection::new(&hyper, &db, CreateMode::CreateAndReplace).expect("connect");

    conn.execute_command(
        "CREATE TABLE users (id BIGINT, name TEXT, email TEXT, score DOUBLE PRECISION)",
    )
    .expect("create users");

    println!("\n=== SPIKE 4: LIMIT 0 dry-run schema ===");

    for (label, sql) in [
        ("plain LIMIT 0", "SELECT id, name, email FROM users LIMIT 0"),
        (
            "CTE wrapper",
            "WITH __hdb_q AS (SELECT id, name, email FROM users) SELECT * FROM __hdb_q LIMIT 0",
        ),
        ("SELECT *", "SELECT * FROM users LIMIT 0"),
        ("expression no FROM", "SELECT 1 AS a, 'x' AS b LIMIT 0"),
    ] {
        match conn.execute_query(sql) {
            Ok(mut rs) => {
                // Drive the stream once: on TCP the schema (RowDescription)
                // only materializes after the first next_chunk() pulls bytes.
                // LIMIT 0 yields Ok(None) but populates the schema cache.
                match rs.next_chunk() {
                    Ok(_) => {}
                    Err(e) => {
                        println!("[{label}] drain ERROR: {e}");
                        continue;
                    }
                }
                match rs.schema() {
                    Some(s) => {
                        let cols: Vec<String> = s
                            .columns()
                            .iter()
                            .map(|c| format!("{}:{:?}", c.name(), c.sql_type()))
                            .collect();
                        println!("[{label}] {} cols -> {}", s.column_count(), cols.join(", "));
                    }
                    None => println!("[{label}] schema() returned None AFTER drain (!)"),
                }
            }
            Err(e) => println!("[{label}] submit ERROR: {e}"),
        }
    }
    println!("(if column names + SqlTypes come back on zero rows, the dry-run mechanism works)\n");
}
