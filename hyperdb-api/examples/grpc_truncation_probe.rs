// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Probe: is there really a 20M-row gRPC cutoff, and if so, what triggers it?
//!
//! Runs `SELECT i, (i%10), (i*0.1), (1700000000000+i*1000) FROM generate_series(1, N)`
//! over gRPC in SYNC mode for N = 100M. Prints timing + row-count + which gRPC
//! path delivered the rows. Read the paired hyperd log to see which server
//! code path terminated the stream.

#![allow(
    clippy::cast_precision_loss,
    reason = "example diagnostic output; bounded experiment sizes"
)]
// Diagnostic probe: row-count accumulator uses i64 for display; usize→i64
// narrowing is fine for the bounded row counts produced here.
#![expect(
    clippy::cast_possible_wrap,
    reason = "probe harness: row counts are bounded well below i64::MAX"
)]

use hyperdb_api::{
    grpc::TransferMode, Connection, ConnectionBuilder, CreateMode, HyperProcess, ListenMode,
    Parameters,
};
use std::net::TcpListener;

fn bind_ephemeral_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn main() -> hyperdb_api::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let row_count: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000_000);

    std::fs::create_dir_all("test_results/bench_probe").ok();

    let grpc_port = bind_ephemeral_port();
    let mut params = Parameters::new();
    params.set("log_dir", "test_results/bench_probe");
    params.set_listen_mode(ListenMode::Both { grpc_port });

    let hyper = HyperProcess::new(None, Some(&params))?;
    let grpc_url = hyper
        .grpc_url()
        .ok_or_else(|| hyperdb_api::Error::internal("no gRPC URL"))?
        .clone();

    let query = format!(
        "SELECT i::INT AS id, (i % 10)::INT AS sensor_id, \
         (i::DOUBLE PRECISION * 0.1) AS value, \
         (1700000000000::BIGINT + i::BIGINT * 1000) AS timestamp \
         FROM generate_series(1, {row_count}) AS s(i)"
    );

    println!("Row count requested: {row_count}");
    println!("gRPC URL: {grpc_url}");
    println!();

    for mode in [
        TransferMode::Sync,
        TransferMode::Adaptive,
        TransferMode::Async,
    ] {
        let label = format!("{mode:?}");
        let connection = ConnectionBuilder::new(&grpc_url)
            .create_mode(CreateMode::DoNotCreate)
            .transfer_mode(mode)
            .build()?;
        let t0 = std::time::Instant::now();
        let mut result = Connection::execute_query(&connection, &query)?;
        let mut rows = 0i64;
        let mut chunks = 0u64;
        while let Some(chunk) = result.next_chunk()? {
            chunks += 1;
            rows += chunk.len() as i64;
        }
        let elapsed = t0.elapsed();
        let secs = elapsed.as_secs_f64();
        // Schema: INT (4) + INT (4) + DOUBLE (8) + BIGINT (8) = 24 bytes/row,
        // no nulls so no validity bitmaps — body size is exact.
        const BYTES_PER_ROW: i64 = 24;
        let body_bytes = rows * BYTES_PER_ROW;
        println!(
            "[{label:>8}] {rows:>13} rows in {chunks:>5} chunks, {secs:.3}s \
             ({:.1}M rows/s, {:.1} MB body, {:.2} GB/s)",
            rows as f64 / secs / 1_000_000.0,
            body_bytes as f64 / (1024.0 * 1024.0),
            body_bytes as f64 / secs / 1e9,
        );
    }

    println!();
    println!("Inspect: test_results/bench_probe/hyperd.log");
    Ok(())
}
