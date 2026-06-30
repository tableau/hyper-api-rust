// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for the connection pools (async [`Pool`] and sync
//! [`ConnectionPool`]), exercising the new option surface added for issue #67:
//! timeouts, lifetime/idle caps, and the configurable recycle strategy.
//!
//! These run against a live Hyper server launched per test.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::{test_hyper_params, test_result_path};
use hyperdb_api::pool::{
    create_pool, ConnectionPool, PoolConfig, RecycleStrategy, SyncPoolConfig, SyncRecycleStrategy,
};
use hyperdb_api::{CreateMode, HyperProcess, Result};

/// Launches a fresh Hyper server and returns it plus its endpoint string.
fn fresh_server(name: &str) -> Result<(HyperProcess, String)> {
    let params = test_hyper_params(name)?;
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper.require_endpoint()?.to_string();
    Ok((hyper, endpoint))
}

fn db_path(name: &str) -> String {
    test_result_path(name, "hyper")
        .expect("result path")
        .to_string_lossy()
        .to_string()
}

/// A connection's stable, unique identity, used by the "was it replaced?"
/// assertions below.
///
/// `process_id()` is unusable here: the wire-level `BackendKeyData` message
/// that populates it is optional in the protocol and the embedded test server
/// omits it, so every connection reports `0` (see `hyperdb-api-core`'s
/// `client_tests.rs` — "Hyper may return 0 ... means no backend PID tracking").
/// The `session_identifier` startup parameter is the right discriminator: it
/// arrives in a `ParameterStatus` startup message (which this embedded server
/// always emits for this key), unlike the optional `BackendKeyData` that feeds
/// `process_id`; it is unique per physical connection and stable across reuse of
/// the same connection — so it proves both replacement (`assert_ne!`) and reuse
/// (`assert_eq!`). The non-empty filter guards the one quiet-failure mode: a
/// server that reported `Some("")` would otherwise make a reuse `assert_eq!`
/// pass spuriously; here it panics loudly instead.
async fn async_session_id(conn: &hyperdb_api::AsyncConnection) -> String {
    conn.parameter_status("session_identifier")
        .await
        .filter(|s| !s.is_empty())
        .expect("server must report a non-empty session_identifier")
}

fn sync_session_id(conn: &hyperdb_api::Connection) -> String {
    conn.parameter_status("session_identifier")
        .filter(|s| !s.is_empty())
        .expect("server must report a non-empty session_identifier")
}

// ---------------------------------------------------------------------------
// Async pool
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_basic_roundtrip() {
    let (_hyper, endpoint) = fresh_server("pool_async_basic").unwrap();
    let config = PoolConfig::new(&endpoint, db_path("pool_async_basic"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(4);
    let pool = create_pool(config).unwrap();

    let conn = pool.get().await.expect("get connection");
    conn.execute_command("CREATE TABLE t (v INT NOT NULL)")
        .await
        .unwrap();
    conn.execute_command("INSERT INTO t VALUES (1)")
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_wait_timeout_fires() {
    let (_hyper, endpoint) = fresh_server("pool_async_wait_to").unwrap();
    // A single-slot pool with a short wait timeout: hold the only connection,
    // then a second concurrent acquire must time out rather than block forever.
    let config = PoolConfig::new(&endpoint, db_path("pool_async_wait_to"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .wait_timeout(Some(Duration::from_millis(300)));
    let pool = create_pool(config).unwrap();

    let held = pool.get().await.expect("first connection");

    let start = std::time::Instant::now();
    let result = pool.get().await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "second acquire should time out");
    assert!(
        elapsed < Duration::from_secs(5),
        "should fail fast via wait_timeout, took {elapsed:?}"
    );
    drop(held);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_recycle_ping_strategy_works() {
    let (_hyper, endpoint) = fresh_server("pool_async_ping").unwrap();
    let config = PoolConfig::new(&endpoint, db_path("pool_async_ping"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(2)
        .recycle(RecycleStrategy::Ping);
    let pool = create_pool(config).unwrap();

    // Acquire, drop, re-acquire: the Ping probe must succeed across checkouts.
    {
        let conn = pool.get().await.expect("first");
        conn.execute_command("SELECT 1").await.unwrap();
    }
    let conn = pool.get().await.expect("second after recycle");
    conn.execute_command("SELECT 1").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_custom_recycle_failure_replaces_connection() {
    let (_hyper, endpoint) = fresh_server("pool_async_custom_fail").unwrap();

    // A custom recycle check that fails the first time it runs (i.e. on the
    // first recycle of a reused connection), forcing the pool to evict and
    // rebuild. Subsequent checks pass.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_in_hook = Arc::clone(&calls);
    let config = PoolConfig::new(&endpoint, db_path("pool_async_custom_fail"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .recycle(RecycleStrategy::Custom(Arc::new(move |_conn| {
            let calls = Arc::clone(&calls_in_hook);
            Box::pin(async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(hyperdb_api::Error::internal("forced recycle failure"))
                } else {
                    Ok(())
                }
            })
        })));
    let pool = create_pool(config).unwrap();

    // First checkout creates a connection (recycle does NOT run on create).
    let sid1 = {
        let conn = pool.get().await.expect("first");
        async_session_id(&conn).await
    };
    // Second checkout recycles: the first recycle fails, so the pool evicts
    // that connection and hands us a freshly built one (different session).
    let conn2 = pool.get().await.expect("second");
    let sid2 = async_session_id(&conn2).await;

    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "custom recycle check must have run"
    );
    assert_ne!(
        sid1, sid2,
        "failed recycle must replace the connection with a new session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_max_lifetime_retires_connection() {
    let (_hyper, endpoint) = fresh_server("pool_async_lifetime").unwrap();
    let config = PoolConfig::new(&endpoint, db_path("pool_async_lifetime"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .max_lifetime(Some(Duration::from_millis(50)));
    let pool = create_pool(config).unwrap();

    let sid1 = {
        let conn = pool.get().await.expect("first");
        async_session_id(&conn).await
    };
    // Let the connection age past max_lifetime, then re-acquire: recycle must
    // retire the aged connection and build a fresh one.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let conn2 = pool.get().await.expect("second");
    assert_ne!(
        sid1,
        async_session_id(&conn2).await,
        "connection past max_lifetime must be replaced"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_idle_timeout_retires_connection() {
    let (_hyper, endpoint) = fresh_server("pool_async_idle").unwrap();
    let config = PoolConfig::new(&endpoint, db_path("pool_async_idle"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .idle_timeout(Some(Duration::from_millis(50)));
    let pool = create_pool(config).unwrap();

    let sid1 = {
        let conn = pool.get().await.expect("first");
        async_session_id(&conn).await
    };
    tokio::time::sleep(Duration::from_millis(120)).await;
    let conn2 = pool.get().await.expect("second");
    assert_ne!(
        sid1,
        async_session_id(&conn2).await,
        "connection past idle_timeout must be replaced"
    );
}

// ---------------------------------------------------------------------------
// Sync pool
// ---------------------------------------------------------------------------

fn sync_pool(name: &str, endpoint: &str) -> ConnectionPool {
    SyncPoolConfig::new(endpoint, db_path(name))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(4)
        .build()
}

#[test]
fn sync_pool_basic_roundtrip() {
    let (_hyper, endpoint) = fresh_server("pool_sync_basic").unwrap();
    let pool = sync_pool("pool_sync_basic", &endpoint);

    let conn = pool.get().expect("get connection");
    conn.execute_command("CREATE TABLE t (v INT NOT NULL)")
        .unwrap();
    conn.execute_command("INSERT INTO t VALUES (42)").unwrap();
    drop(conn);

    // Reuse: the returned connection should be recycled and handed back out.
    let conn2 = pool.get().expect("reuse connection");
    let n: i64 = conn2
        .execute_scalar_query::<i64>("SELECT COUNT(*) FROM t")
        .unwrap()
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        pool.status().size,
        1,
        "pool should reuse the one connection"
    );
}

#[test]
fn sync_pool_wait_timeout_fires() {
    let (_hyper, endpoint) = fresh_server("pool_sync_wait_to").unwrap();
    let pool = SyncPoolConfig::new(&endpoint, db_path("pool_sync_wait_to"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .wait_timeout(Some(Duration::from_millis(200)))
        .build();

    let held = pool.get().expect("first connection");

    let start = std::time::Instant::now();
    let result = pool.get();
    let elapsed = start.elapsed();

    assert!(result.is_err(), "second acquire should time out");
    assert!(
        matches!(result, Err(hyperdb_api::Error::Timeout(_))),
        "should be a Timeout error"
    );
    assert!(
        elapsed >= Duration::from_millis(150),
        "should wait roughly the timeout, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "should not block far past the timeout, took {elapsed:?}"
    );
    drop(held);
}

#[test]
fn sync_pool_idle_timeout_retires_connection() {
    let (_hyper, endpoint) = fresh_server("pool_sync_idle").unwrap();
    // max_size(1): a differing session id after the idle cap can only mean the
    // one connection was retired and rebuilt — a second connection can't coexist.
    let pool = SyncPoolConfig::new(&endpoint, db_path("pool_sync_idle"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .idle_timeout(Some(Duration::from_millis(50)))
        .build();

    let sid1 = {
        let conn = pool.get().expect("first");
        sync_session_id(&conn)
    };
    std::thread::sleep(Duration::from_millis(120));
    let conn2 = pool.get().expect("second");
    assert_ne!(
        sid1,
        sync_session_id(&conn2),
        "connection past idle_timeout must be replaced"
    );
}

#[test]
fn sync_pool_max_lifetime_retires_connection() {
    let (_hyper, endpoint) = fresh_server("pool_sync_lifetime").unwrap();
    // max_size(1): a differing session id after the lifetime cap can only mean the
    // one connection was retired and rebuilt — a second connection can't coexist.
    let pool = SyncPoolConfig::new(&endpoint, db_path("pool_sync_lifetime"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .max_lifetime(Some(Duration::from_millis(50)))
        .build();

    let sid1 = {
        let conn = pool.get().expect("first");
        sync_session_id(&conn)
    };
    std::thread::sleep(Duration::from_millis(120));
    let conn2 = pool.get().expect("second");
    assert_ne!(
        sid1,
        sync_session_id(&conn2),
        "connection past max_lifetime must be replaced"
    );
}

#[test]
fn sync_pool_custom_recycle_failure_replaces_connection() {
    let (_hyper, endpoint) = fresh_server("pool_sync_custom_fail").unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_in_hook = Arc::clone(&calls);
    let pool = SyncPoolConfig::new(&endpoint, db_path("pool_sync_custom_fail"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .recycle(SyncRecycleStrategy::Custom(Arc::new(move |_conn| {
            let n = calls_in_hook.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(hyperdb_api::Error::internal("forced recycle failure"))
            } else {
                Ok(())
            }
        })))
        .build();

    let sid1 = {
        let conn = pool.get().expect("first");
        sync_session_id(&conn)
    };
    let conn2 = pool.get().expect("second");
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "custom recycle check must have run"
    );
    assert_ne!(
        sid1,
        sync_session_id(&conn2),
        "failed recycle must replace the connection"
    );
}

#[test]
fn sync_pool_recycle_none_skips_probe() {
    let (_hyper, endpoint) = fresh_server("pool_sync_none").unwrap();
    let pool = SyncPoolConfig::new(&endpoint, db_path("pool_sync_none"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .recycle(SyncRecycleStrategy::None)
        .build();

    let sid1 = {
        let conn = pool.get().expect("first");
        conn.execute_command("CREATE TABLE t (v INT NOT NULL)")
            .unwrap();
        sync_session_id(&conn)
    };
    // With no active probe, the still-alive connection is reused as-is.
    let conn2 = pool.get().expect("reuse");
    assert_eq!(
        sid1,
        sync_session_id(&conn2),
        "live connection should be reused"
    );
}

#[test]
fn sync_pool_concurrent_threads_share_pool() {
    let (_hyper, endpoint) = fresh_server("pool_sync_concurrent").unwrap();
    let pool = SyncPoolConfig::new(&endpoint, db_path("pool_sync_concurrent"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(4)
        .build();

    // Set up a table via the pool.
    {
        let conn = pool.get().expect("setup");
        conn.execute_command("CREATE TABLE counters (id INT NOT NULL)")
            .unwrap();
    }

    let mut handles = Vec::new();
    for i in 0..8 {
        let pool = pool.clone();
        handles.push(std::thread::spawn(move || {
            let conn = pool.get().expect("worker get");
            conn.execute_command(&format!("INSERT INTO counters VALUES ({i})"))
                .expect("insert");
        }));
    }
    for h in handles {
        h.join().expect("thread join");
    }

    let conn = pool.get().expect("final");
    let n: i64 = conn
        .execute_scalar_query::<i64>("SELECT COUNT(*) FROM counters")
        .unwrap()
        .unwrap();
    assert_eq!(n, 8, "all 8 concurrent inserts should have landed");
    assert!(
        pool.status().size <= 4,
        "pool must never exceed max_size, got {}",
        pool.status().size
    );
}

#[test]
fn sync_pool_take_frees_slot() {
    let (_hyper, endpoint) = fresh_server("pool_sync_take").unwrap();
    let pool = SyncPoolConfig::new(&endpoint, db_path("pool_sync_take"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(2)
        .build();

    let conn = pool.get().expect("get");
    let owned = conn.take();
    // Taken connection still works and the pool slot was released.
    owned.execute_command("SELECT 1").unwrap();
    assert_eq!(pool.status().size, 0, "take() should free the pool slot");
}
