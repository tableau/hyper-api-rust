// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for the gRPC streaming chunk API.
//!
//! These tests verify that `GrpcClientSync::execute_query_stream` and the
//! streaming [`ArrowRowset::from_stream`] path wire up correctly against a
//! live `hyperd`:
//!
//! - the raw `GrpcClientSync` streaming API yields one or more `Bytes`
//!   chunks, and the streaming `ArrowRowset::from_stream` path decodes them
//!   to the full row count.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hyperdb_api::{HyperProcess, ListenMode, Parameters, Result};

/// Spins up `hyperd` in gRPC-only mode and returns the process and its gRPC URL.
fn grpc_hyperd() -> Result<(HyperProcess, String)> {
    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    params.set_listen_mode(ListenMode::Grpc { port: 0 });
    let hyper = HyperProcess::new(None, Some(&params))?;
    let url = hyper
        .grpc_url()
        .expect("hyperd should expose a gRPC URL when listen_mode=Grpc");
    Ok((hyper, url))
}

/// Exercises the low-level `GrpcClientSync::execute_query_stream` and checks
/// that the chunks round-trip through a streaming `ArrowRowset` for the
/// correct row count.
#[test]
fn test_grpc_stream_chunks_decode_correctly() -> Result<()> {
    let (_hyper, url) = grpc_hyperd()?;
    let config = hyperdb_api_core::client::grpc::GrpcConfig::new(&url);
    let mut client = hyperdb_api_core::client::grpc::GrpcClientSync::connect(config)?;

    // A generate_series result is cheap and predictable — 50k rows.
    let stream = client.execute_query_stream("SELECT i FROM generate_series(1, 50000) AS s(i)")?;

    // Count how many raw Bytes chunks the server produced.
    struct Counter {
        inner: hyperdb_api_core::client::grpc::GrpcChunkStreamSync,
        seen: Arc<AtomicUsize>,
    }
    impl hyperdb_api::ChunkSource for Counter {
        fn next_chunk(&mut self) -> hyperdb_api::Result<Option<bytes::Bytes>> {
            match self.inner.next_chunk()? {
                Some(b) => {
                    self.seen.fetch_add(1, Ordering::Relaxed);
                    Ok(Some(b))
                }
                None => Ok(None),
            }
        }
    }

    let chunks = Arc::new(AtomicUsize::new(0));
    let source = Box::new(Counter {
        inner: stream,
        seen: Arc::clone(&chunks),
    });
    let mut rowset = hyperdb_api::ArrowRowset::from_stream(source)?;

    let mut total_rows = 0usize;
    while let Some(chunk) = rowset.next_chunk()? {
        total_rows += chunk.len();
    }
    assert_eq!(total_rows, 50_000, "streamed row count must match query");
    assert!(
        chunks.load(Ordering::Relaxed) >= 1,
        "server produced no chunks"
    );
    Ok(())
}

// The high-level `Connection::execute_query` over gRPC is exercised end-to-end
// by the TCP-vs-gRPC benchmark phase in `benches/benchmark.rs`, which
// populates a real `measurements` table via TCP and queries it via gRPC —
// covering both the streaming wiring and a realistic data volume.
