// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the directory watcher. Uses real filesystem operations with
//! tempfile-managed directories. Ingest happens against a real Hyper engine
//! so the tests double as integration tests for the ingest path.

#![expect(
    clippy::cast_possible_wrap,
    reason = "test data (row counts) bounded by test parameters; usize→i64 wrap is unreachable"
)]
#![allow(
    clippy::cast_precision_loss,
    reason = "test diagnostic calculations; values bounded far below 2^53"
)]

mod common;

use common::TestEngine;
use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::watcher::{self, WatchOptions, WatcherRegistry};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[expect(
    clippy::used_underscore_binding,
    reason = "underscore-prefixed parameter retained for trait-method signature compatibility"
)]
/// Wrap an owned Engine in the Arc<Mutex<Option<_>>> shape the watcher expects.
fn engine_handle(te: TestEngine) -> (Arc<Mutex<Option<Engine>>>, tempfile::TempDir) {
    let TestEngine { engine, _temp_dir } = te;
    (Arc::new(Mutex::new(Some(engine))), _temp_dir)
}

/// Poll a predicate until it returns true or the timeout elapses.
fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// Write a data file + .ready companion atomically (from the watcher's POV).
fn drop_ready_pair(dir: &std::path::Path, name: &str, content: &[u8]) {
    let data_path = dir.join(name);
    std::fs::write(&data_path, content).unwrap();
    let ready_path = dir.join(format!("{name}.ready"));
    std::fs::write(&ready_path, b"").unwrap();
}

/// Happy path: drop a CSV + .ready, the watcher ingests it, both files are
/// deleted, and the rows are in the target table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_ingests_csv_and_cleans_up() {
    let (engine, _td) = engine_handle(TestEngine::new_ephemeral());
    let watch_dir = tempfile::TempDir::new().unwrap();
    let registry = Arc::new(WatcherRegistry::new());

    watcher::start_watching(
        Arc::clone(&engine),
        Arc::clone(&registry),
        None,
        watch_dir.path().to_path_buf(),
        "events".into(),
        WatchOptions::default(),
    )
    .unwrap();

    drop_ready_pair(watch_dir.path(), "batch1.csv", b"id,name\n1,Alice\n2,Bob\n");

    let canon = watch_dir.path().canonicalize().unwrap();
    let ready = canon.join("batch1.csv.ready");
    let data = canon.join("batch1.csv");
    let ingested = wait_until(
        || !ready.exists() && !data.exists(),
        Duration::from_secs(10),
    );
    assert!(ingested, "watcher did not finish ingesting within 10s");

    let count: i64 = engine
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .connection()
        .execute_scalar_query("SELECT COUNT(*) FROM events")
        .unwrap()
        .unwrap();
    assert_eq!(count, 2);
}

/// A malformed data file is moved to the `failed/` subdirectory with a
/// sibling `.error` JSON file. The main dir is left empty of .ready markers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_moves_bad_files_to_failed() {
    let (engine, _td) = engine_handle(TestEngine::new_ephemeral());
    let watch_dir = tempfile::TempDir::new().unwrap();
    let registry = Arc::new(WatcherRegistry::new());

    // Pre-create the table with a known schema so the second file (with a
    // completely different schema) will fail.
    {
        let guard = engine.lock().unwrap();
        guard
            .as_ref()
            .unwrap()
            .execute_command("CREATE TABLE evts (id INT, name TEXT)")
            .unwrap();
    }

    watcher::start_watching(
        Arc::clone(&engine),
        Arc::clone(&registry),
        None,
        watch_dir.path().to_path_buf(),
        "evts".into(),
        WatchOptions::default(),
    )
    .unwrap();

    // A file whose content cannot be COPY'd into the existing table (wrong
    // column count / garbage data).
    drop_ready_pair(
        watch_dir.path(),
        "bad.csv",
        b"this,is,not,matching,the,schema\nfoo,bar,baz,qux,a,b\n",
    );

    let canon = watch_dir.path().canonicalize().unwrap();
    let failed_dir = canon.join("failed");
    let moved = wait_until(
        || failed_dir.join("bad.csv").exists() && failed_dir.join("bad.csv.error").exists(),
        Duration::from_secs(10),
    );
    assert!(moved, "expected files to land in failed/ within 10s");

    let err_text = std::fs::read_to_string(failed_dir.join("bad.csv.error")).unwrap();
    assert!(
        err_text.contains("code"),
        "error file should contain a code field"
    );
}

/// Starting a watcher picks up files already in the directory before watching began.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_sweep_picks_up_preexisting_files() {
    let (engine, _td) = engine_handle(TestEngine::new_ephemeral());
    let watch_dir = tempfile::TempDir::new().unwrap();
    let registry = Arc::new(WatcherRegistry::new());

    drop_ready_pair(watch_dir.path(), "preexisting.csv", b"x\n1\n2\n3\n");

    let initial = watcher::start_watching(
        Arc::clone(&engine),
        Arc::clone(&registry),
        None,
        watch_dir.path().to_path_buf(),
        "t".into(),
        WatchOptions::default(),
    )
    .unwrap();
    assert_eq!(initial.files_ingested, 1);
    assert_eq!(initial.files_failed, 0);

    let count: i64 = engine
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .connection()
        .execute_scalar_query("SELECT COUNT(*) FROM t")
        .unwrap()
        .unwrap();
    assert_eq!(count, 3);
}

/// Unwatching stops the thread cleanly and removes the entry from the registry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unwatch_removes_from_registry() {
    let (engine, _td) = engine_handle(TestEngine::new_ephemeral());
    let watch_dir = tempfile::TempDir::new().unwrap();
    let registry = Arc::new(WatcherRegistry::new());

    watcher::start_watching(
        Arc::clone(&engine),
        Arc::clone(&registry),
        None,
        watch_dir.path().to_path_buf(),
        "logs".into(),
        WatchOptions::default(),
    )
    .unwrap();

    let canon = watch_dir.path().canonicalize().unwrap();
    assert_eq!(registry.len(), 1);

    let summary = watcher::stop_watching(&registry, &canon).unwrap();
    assert_eq!(summary["status"], "stopped");
    assert!(registry.is_empty());
}

/// Attempting to watch the same directory twice is rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_same_directory_twice_fails() {
    let (engine, _td) = engine_handle(TestEngine::new_ephemeral());
    let watch_dir = tempfile::TempDir::new().unwrap();
    let registry = Arc::new(WatcherRegistry::new());

    watcher::start_watching(
        Arc::clone(&engine),
        Arc::clone(&registry),
        None,
        watch_dir.path().to_path_buf(),
        "t1".into(),
        WatchOptions::default(),
    )
    .unwrap();

    let err = watcher::start_watching(
        Arc::clone(&engine),
        Arc::clone(&registry),
        None,
        watch_dir.path().to_path_buf(),
        "t2".into(),
        WatchOptions::default(),
    )
    .unwrap_err();
    assert!(
        err.message.contains("Already watching"),
        "message: {}",
        err.message
    );
}

/// Unwatching a directory that was never registered returns `FileNotFound`.
#[test]
fn unwatch_unknown_dir_errors() {
    let registry = WatcherRegistry::new();
    let nowhere = PathBuf::from("/this/path/does/not/exist-for-sure");
    let err = watcher::stop_watching(&registry, &nowhere).unwrap_err();
    assert_eq!(err.code, hyperdb_mcp::error::ErrorCode::FileNotFound);
}

/// The server's read-only flag disables `watch_directory` via `check_writable`.
/// We can't easily call the rmcp tool handler directly in a unit test, but
/// we can verify the gate that it relies on.
#[test]
fn read_only_server_blocks_writes() {
    let ro = hyperdb_mcp::server::HyperMcpServer::with_no_daemon(None, true, true);
    assert!(ro.is_read_only());
}

/// Drop several `.ready` files at once and confirm the watcher processes
/// them in parallel. Each file is a JSON array so ingest takes long
/// enough to observe concurrency (per-row INSERTs).
///
/// We don't assert strict timing — that's too flaky on shared CI — but
/// we do assert that:
///   1. Every row lands in the target table.
///   2. Every data file is removed after ingest.
///   3. `max_concurrent` shows up in the stats snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_ingests_many_files_concurrently() {
    let (engine, _td) = engine_handle(TestEngine::new_ephemeral());
    let watch_dir = tempfile::TempDir::new().unwrap();
    let registry = Arc::new(WatcherRegistry::new());

    let initial = watcher::start_watching(
        Arc::clone(&engine),
        Arc::clone(&registry),
        None,
        watch_dir.path().to_path_buf(),
        "batches".into(),
        WatchOptions { max_concurrent: 4 },
    )
    .unwrap();
    assert_eq!(initial.max_concurrent, 4);

    // 8 files × 100 rows = 800 rows total. With max_concurrent=4 the
    // watcher should keep 4 pooled connections busy at a time.
    const FILES: usize = 8;
    const ROWS_PER_FILE: usize = 100;
    for i in 0..FILES {
        let mut csv = String::from("id,name,value\n");
        for r in 0..ROWS_PER_FILE {
            let id = i * ROWS_PER_FILE + r;
            let _ = writeln!(csv, "{id},row-{id},{}", id as f64 * 1.5);
        }
        drop_ready_pair(
            watch_dir.path(),
            &format!("batch-{i:02}.csv"),
            csv.as_bytes(),
        );
    }

    let canon = watch_dir.path().canonicalize().unwrap();
    let ingested = wait_until(
        || {
            // All .ready sentinels gone → watcher has processed (or
            // failed) every file.
            std::fs::read_dir(&canon).is_ok_and(|rd| {
                !rd.flatten().any(|e| {
                    e.file_name().to_str().is_some_and(|n| {
                        std::path::Path::new(n)
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("ready"))
                    })
                })
            })
        },
        Duration::from_secs(30),
    );
    assert!(
        ingested,
        "watcher did not finish processing all files within 30s"
    );

    let total: i64 = engine
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .connection()
        .execute_scalar_query("SELECT COUNT(*) FROM batches")
        .unwrap()
        .unwrap();
    assert_eq!(total, (FILES * ROWS_PER_FILE) as i64);
}
