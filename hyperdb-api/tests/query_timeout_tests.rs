// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `query_timeout` on both connection builders is enforced by the server.

mod common;

use std::time::{Duration, Instant};

use common::test_hyper_params;
use hyperdb_api::{AsyncConnectionBuilder, ConnectionBuilder, HyperProcess};

/// A cross join large enough to run for far longer than the timeout.
const SLOW_QUERY: &str = "SELECT COUNT(*) FROM generate_series(1, 20000) AS a(x), \
                          generate_series(1, 100000) AS b(y) WHERE (a.x::BIGINT * b.y) % 7 = 3";

/// Whether `err` is the server's query-timeout cancellation.
///
/// hyperd's wording changed between releases: older engines report the
/// statement as "canceled", while 0.0.27426 reports `53S01` "The query
/// exceeded the time limit of N milliseconds". The SQLSTATE is the stable part,
/// so accept it alongside either message.
fn is_timeout_error(err: &hyperdb_api::Error) -> bool {
    let text = err.to_string().to_lowercase();
    text.contains("cancel") || text.contains("53s01") || text.contains("exceeded the time limit")
}

#[test]
fn sync_query_timeout_cancels_slow_query() {
    let params = test_hyper_params("sync_query_timeout").unwrap();
    let hyper = HyperProcess::new(None, Some(&params)).unwrap();
    let endpoint = hyper.require_endpoint().unwrap().to_string();

    let conn = ConnectionBuilder::new(&endpoint)
        .query_timeout(Duration::from_millis(100))
        .build()
        .unwrap();

    let start = Instant::now();
    let result: hyperdb_api::Result<i64> = conn.fetch_scalar(SLOW_QUERY);
    eprintln!("ELAPSED {:?}", start.elapsed());
    let err = result.expect_err("slow query should be cancelled");
    assert!(is_timeout_error(&err), "unexpected error: {err}");
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "query ran for {:?}, timeout not enforced",
        start.elapsed()
    );

    // The connection remains usable and fast queries still succeed.
    let one: i64 = conn.fetch_scalar("SELECT 1").unwrap();
    assert_eq!(one, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn async_query_timeout_cancels_slow_query() {
    let params = test_hyper_params("async_query_timeout").unwrap();
    let hyper = HyperProcess::new(None, Some(&params)).unwrap();
    let endpoint = hyper.require_endpoint().unwrap().to_string();

    let conn = AsyncConnectionBuilder::new(&endpoint)
        .query_timeout(Duration::from_millis(100))
        .build()
        .await
        .unwrap();

    let start = Instant::now();
    let result: hyperdb_api::Result<i64> = conn.fetch_scalar(SLOW_QUERY).await;
    eprintln!("ELAPSED {:?}", start.elapsed());
    let err = result.expect_err("slow query should be cancelled");
    assert!(is_timeout_error(&err), "unexpected error: {err}");
    assert!(start.elapsed() < Duration::from_secs(30));
}

#[test]
fn zero_query_timeout_is_rejected() {
    let params = test_hyper_params("zero_query_timeout").unwrap();
    let hyper = HyperProcess::new(None, Some(&params)).unwrap();
    let endpoint = hyper.require_endpoint().unwrap().to_string();

    let result = ConnectionBuilder::new(&endpoint)
        .query_timeout(Duration::ZERO)
        .build();
    assert!(result.is_err());
}
