// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for the connection pools (async [`Pool`] and sync
//! [`ConnectionPool`]), exercising the new option surface added for issue #67:
//! timeouts, lifetime/idle caps, and the configurable recycle strategy.
//!
//! These run against a live Hyper server launched per test.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{test_hyper_params, test_result_path};
use hyperdb_api::pool::{
    ConnectionPool, PoolConfig, RecycleStrategy, SyncPoolConfig, SyncRecycleStrategy, create_pool,
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

    assert!(
        matches!(result, Err(hyperdb_api::Error::Timeout(_))),
        "second acquire should time out with Error::Timeout, got {:?}",
        result.as_ref().err()
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "should fail fast via wait_timeout, took {elapsed:?}"
    );
    drop(held);
}

/// `Pool::status` reports occupancy through `PoolStatus`, mirroring the sync
/// pool, and `PooledConnection::take` frees the slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_status_and_take() {
    let (_hyper, endpoint) = fresh_server("pool_async_status").unwrap();
    let config = PoolConfig::new(&endpoint, db_path("pool_async_status"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(3);
    let pool = create_pool(config).unwrap();

    let status = pool.status();
    assert_eq!((status.size, status.idle, status.max_size), (0, 0, 3));

    let conn = pool.get().await.expect("get");
    let status = pool.status();
    assert_eq!((status.size, status.idle), (1, 0), "one checked out");

    drop(conn);
    let status = pool.status();
    assert_eq!((status.size, status.idle), (1, 1), "returned to the pool");

    let owned = pool.get().await.expect("get again").take();
    owned.execute_command("SELECT 1").await.unwrap();
    assert_eq!(pool.status().size, 0, "take() should free the pool slot");
}

/// A closed pool refuses checkouts with a `hyperdb_api::Error`, not a
/// deadpool error type. It is `InvalidOperation` (retrying cannot succeed),
/// not `Connection` (which callers treat as transient).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_get_after_close_is_invalid_operation() {
    let (_hyper, endpoint) = fresh_server("pool_async_closed").unwrap();
    let config = PoolConfig::new(&endpoint, db_path("pool_async_closed"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(2);
    let pool = create_pool(config).unwrap();
    assert!(!pool.is_closed());

    pool.close();
    assert!(pool.is_closed());
    let err = pool
        .get()
        .await
        .expect_err("closed pool must refuse checkout");
    assert!(
        matches!(err, hyperdb_api::Error::InvalidOperation(_)),
        "expected Error::InvalidOperation, got {err:?}"
    );
}

/// A failure to open a connection surfaces as the underlying
/// `hyperdb_api::Error`, not a wrapped pool error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_get_surfaces_connect_error() {
    // Nothing listens on port 1; the first checkout has to open a connection.
    let config = PoolConfig::new("127.0.0.1:1", db_path("pool_async_connect_err"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        // Guard only: refusal is immediate on Linux and macOS, but a
        // filtering firewall or Windows' SYN retries would otherwise stall.
        .create_timeout(Some(Duration::from_secs(30)));
    let pool = create_pool(config).unwrap();

    let err = pool.get().await.expect_err("nothing is listening");
    assert!(
        matches!(err, hyperdb_api::Error::Connection { .. }),
        "expected Error::Connection, got {err:?}"
    );
}

/// A `create_timeout` that elapses is `Error::Timeout`. The listener accepts
/// the TCP connection (the kernel does, via the backlog) but never answers
/// the startup message, so the connect hangs until the timeout fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_create_timeout_is_timeout_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let config = PoolConfig::new(addr.to_string(), db_path("pool_async_create_timeout"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1)
        .create_timeout(Some(Duration::from_millis(300)));
    let pool = create_pool(config).unwrap();

    let err = pool.get().await.expect_err("the server never answers");
    assert!(
        matches!(err, hyperdb_api::Error::Timeout(_)),
        "expected Error::Timeout, got {err:?}"
    );
    drop(listener);
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

/// Regression test for [issue #263](https://github.com/tableau/hyper-api-rust/issues/263):
/// a connection returned to the pool with an open transaction (left by a
/// panicked or cancelled task holding an `AsyncTransaction`) must not be
/// handed to the next borrower still mid-transaction.
///
/// `AsyncTransaction::drop` cannot issue an async `ROLLBACK` (Rust has no
/// async `Drop`), so it only warns when dropped without an explicit
/// `commit()`/`rollback()` — exactly what happens when the task holding the
/// guard panics, mirroring the real call sites in `ingest.rs` /
/// `ingest_arrow.rs`, which already catch the `tokio::spawn` join error and
/// continue rather than propagating a panic.
///
/// `max_size(1)` caps the pool at one physical connection at a time, but it
/// does **not** by itself prove the fix: deadpool evicts and builds a
/// replacement on any `RecycleError`, and a fresh session cannot see another
/// session's uncommitted row, so the `count == 0` assertion below would pass
/// even if the connection had merely been replaced. The
/// [`async_session_id`] comparison is what pins the causal mechanism —
/// identical session ids mean the *same physical connection* was recycled,
/// so `ConnectionManager::recycle` is what discharged the transaction.
///
/// That comparison also covers the cost premise of the fix, which nothing
/// else in this file does: it is the only assertion that `ROLLBACK` *itself
/// succeeds* on a recycled connection. (`async_pool_recycle_ping_strategy_works`
/// uses `Ping`, `async_pool_custom_recycle_failure_replaces_connection` uses
/// `Custom`, and the `max_lifetime` / `idle_timeout` tests return early from
/// `recycle` before the probe runs.) If the probe ever began erroring on an
/// idle connection, the pool would silently rebuild on *every* checkout — a
/// throughput cliff — and `assert_eq!` on the session id is what fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_recycle_discharges_transaction_left_open_by_panicked_task() {
    let (_hyper, endpoint) = fresh_server("pool_async_txn_leak").unwrap();
    let config = PoolConfig::new(&endpoint, db_path("pool_async_txn_leak"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1);
    let pool = create_pool(config).unwrap();

    {
        let conn = pool.get().await.expect("setup checkout");
        conn.execute_command("CREATE TABLE leaked (v INT)")
            .await
            .unwrap();
    }

    // Simulate the defect: BEGIN, write a row, then panic before
    // commit/rollback. The `AsyncTransaction` guard's `Drop` only warns; the
    // transaction is still open on the physical connection when the pooled
    // guard (`conn`) is itself dropped during the same unwind and returns
    // that connection to deadpool's idle set.
    //
    // The `leaked_sid` slot publishes the panicking task's session id so the
    // assertion below can prove the next checkout got that same physical
    // connection back.
    let leaked_sid = Arc::new(Mutex::new(None));
    let pool_for_task = pool.clone();
    let sid_slot = Arc::clone(&leaked_sid);
    let join_result = tokio::spawn(async move {
        let mut conn = pool_for_task.get().await.expect("panic-task checkout");
        let sid = async_session_id(&conn).await;
        *sid_slot.lock().expect("session-id slot") = Some(sid);
        let txn = conn.transaction().await.expect("begin txn");
        txn.execute_command("INSERT INTO leaked VALUES (999)")
            .await
            .expect("insert inside txn");
        panic!("simulated tool handler bug mid-transaction");
    })
    .await;
    assert!(
        join_result.is_err(),
        "spawned task should have panicked, mirroring the real join-error \
         handling in load_files"
    );
    let leaked_sid = leaked_sid
        .lock()
        .expect("session-id slot")
        .clone()
        .expect("panicking task must have recorded its session id before the panic");

    // This checkout runs the leaked connection through
    // `ConnectionManager::recycle`. Without the fix, the old `SELECT 1` probe
    // succeeds despite the open transaction and this borrower silently
    // inherits it; with the fix, `recycle` issues an unconditional
    // `ROLLBACK` first.
    let conn2 = pool.get().await.expect("checkout after panicking task");

    // The load-bearing assertion: same session id means `recycle` succeeded
    // and returned this exact connection to service. A `ROLLBACK` that
    // errored would instead have evicted it, and deadpool would have handed
    // us a replacement with a different id — which would still satisfy the
    // `count == 0` check below, silently hiding both a broken probe and the
    // per-checkout rebuild cliff it would cause.
    assert_eq!(
        leaked_sid,
        async_session_id(&conn2).await,
        "recycle must ROLLBACK and reuse the same physical connection, not \
         evict it and build a replacement"
    );

    // Prove the leaked row did NOT survive — i.e. `recycle` genuinely rolled
    // it back, rather than merely succeeding without touching the
    // transaction.
    let count: i64 = conn2
        .query_count("SELECT COUNT(*) FROM leaked WHERE v = 999")
        .await
        .expect("query after recycle");
    assert_eq!(
        count, 0,
        "recycle must roll back a transaction leaked by a panicked task \
         before handing the connection to the next borrower"
    );

    // And the connection must be immediately usable for ordinary work, not
    // stuck mid-transaction from the next borrower's perspective.
    conn2
        .execute_command("INSERT INTO leaked VALUES (1)")
        .await
        .expect("connection must be usable, not wedged mid-transaction");
    let total: i64 = conn2
        .query_count("SELECT COUNT(*) FROM leaked")
        .await
        .unwrap();
    assert_eq!(total, 1, "only the post-recycle insert should be present");
}

/// The cancellation half of [issue #263](https://github.com/tableau/hyper-api-rust/issues/263),
/// which names "panic and cancellation" — the sibling test above covers only
/// the panic.
///
/// The fix covers cancellation by construction: dropping a pending future
/// drops `AsyncTransaction` and then `PooledConnection` in the same order an
/// unwind does, so it reaches the same `ConnectionManager::recycle`. This
/// test exists so that equivalence is asserted rather than assumed, and so
/// the changelog's "panicked or cancelled" claim is test-backed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_recycle_discharges_transaction_left_open_by_cancelled_task() {
    let (_hyper, endpoint) = fresh_server("pool_async_txn_cancel").unwrap();
    let config = PoolConfig::new(&endpoint, db_path("pool_async_txn_cancel"))
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(1);
    let pool = create_pool(config).unwrap();

    {
        let conn = pool.get().await.expect("setup checkout");
        conn.execute_command("CREATE TABLE leaked (v INT)")
            .await
            .unwrap();
    }

    // Cancel the task by timing it out mid-transaction: `tokio::time::timeout`
    // drops the inner future once the deadline passes, which drops the
    // `AsyncTransaction` (BEGIN still open) and then the pooled connection.
    let leaked_sid = Arc::new(Mutex::new(None));
    let sid_slot = Arc::clone(&leaked_sid);
    let cancelled = tokio::time::timeout(Duration::from_millis(150), async {
        let mut conn = pool.get().await.expect("cancel-task checkout");
        let sid = async_session_id(&conn).await;
        *sid_slot.lock().expect("session-id slot") = Some(sid);
        let txn = conn.transaction().await.expect("begin txn");
        txn.execute_command("INSERT INTO leaked VALUES (999)")
            .await
            .expect("insert inside txn");
        // Outlive the deadline so the future is dropped right here, with the
        // transaction open and never committed or rolled back.
        tokio::time::sleep(Duration::from_secs(30)).await;
        unreachable!("the timeout must fire first");
    })
    .await;
    assert!(cancelled.is_err(), "the task must have been cancelled");
    let leaked_sid = leaked_sid
        .lock()
        .expect("session-id slot")
        .clone()
        .expect("cancelled task must have recorded its session id");

    let conn2 = pool.get().await.expect("checkout after cancelled task");
    assert_eq!(
        leaked_sid,
        async_session_id(&conn2).await,
        "recycle must ROLLBACK and reuse the same physical connection the \
         cancelled task left mid-transaction"
    );
    let count: i64 = conn2
        .query_count("SELECT COUNT(*) FROM leaked WHERE v = 999")
        .await
        .expect("query after recycle");
    assert_eq!(
        count, 0,
        "recycle must roll back a transaction leaked by a cancelled task, \
         exactly as it does for a panicked one"
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
