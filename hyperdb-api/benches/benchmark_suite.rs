// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Unified `hyperdb-api` benchmark suite.
//!
//! Runs the full sync-vs-async matrix against a single `HyperProcess`
//! and emits:
//!
//! 1. A human-readable sync/async comparison table to stdout.
//! 2. A machine-readable JSON file at
//!    `test_results/benchmark_suite.json`.
//! 3. A markdown fragment at `test_results/benchmark_suite.md`
//!    ready to paste into `docs/BENCHMARK_GUIDE.md`.
//!
//! The matrix (all on the canonical `measurements` schema, 24 bytes/row):
//!
//!   Insert:
//!     - sync  · Inserter (`HyperBinary`)           1 connection
//!     - sync  · `ChunkSender` (multi-threaded)     1 connection, N workers
//!     - async · `AsyncArrowInserter`                1 connection
//!     - async · `AsyncArrowInserter` × parallel     N connections
//!     - async · `spawn_blocking` + `ChunkSender` × parallel N connections
//!   Query (streaming, chunked):
//!     - sync  · `full_scan` / filtered / aggregation       1 connection
//!     - async · `full_scan` / filtered / aggregation       1 connection
//!     - async · `full_scan` / filtered / aggregation       N connections
//!
//! Every pair can be read directly to answer "is sync or async faster
//! for workload X at scale Y?".
//!
//! Run with:
//! ```sh
//! HYPERD_PATH=~/dev/bin/hyperd \
//!   cargo run -p hyperdb-api --release --example benchmark_suite
//! # Custom scale (rows per workload) and worker count:
//! HYPERD_PATH=~/dev/bin/hyperd \
//!   cargo run -p hyperdb-api --release --example benchmark_suite -- 20000000 8
//! ```

// Benchmark harness: intentional wide→narrow conversions for row-count display,
// throughput math, and synthetic indexing with bench-enforced bounds.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    reason = "benchmark harness: counts/indices narrow by bench-enforced invariants, throughput math needs f64"
)]

#[path = "common.rs"]
mod common;

use std::env;
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{Float64Array, Int32Array, Int64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;

use hyperdb_api::{
    AsyncArrowInserter, AsyncConnection, ChunkSender, Connection, CreateMode, HyperProcess,
    InsertChunk, Inserter, Parameters, Result, SqlType, TableDefinition, TransportMode,
};

use common::{
    fmt_count, fmt_rate, gen_id, gen_sensor_id, gen_timestamp, gen_value, records_to_json,
    records_to_markdown, BenchRecord, HostEnv, BYTES_PER_ROW, MEASUREMENTS_DDL,
};

const DEFAULT_ROWS_PER_WORKLOAD: i64 = 10_000_000;
const DEFAULT_WORKERS: usize = 4;
const ROWS_PER_BATCH: usize = 100_000;

fn main() -> Result<()> {
    let (rows, workers) = parse_args();

    println!("=== Hyper API Benchmark Suite ===\n");
    let env = HostEnv::detect();
    println!("Host:");
    println!("{}", env.markdown());
    println!();
    println!("Configuration:");
    println!("  Rows per workload:    {}", fmt_count(rows as u64));
    println!("  Parallel workers:     {workers}");
    println!(
        "  Rows per batch:       {}",
        fmt_count(ROWS_PER_BATCH as u64)
    );
    println!(
        "  Schema:               measurements({MEASUREMENTS_DDL}) · {BYTES_PER_ROW} bytes/row"
    );
    println!();

    std::fs::create_dir_all("test_results").ok();

    // Allow `BENCH_TRANSPORT=ipc` to switch to Named Pipe (Windows) or
    // Unix Domain Socket (Unix). Default remains TCP so historical
    // numbers stay comparable.
    let transport = match env::var("BENCH_TRANSPORT").ok().as_deref() {
        Some("ipc" | "IPC" | "pipe") => TransportMode::Ipc,
        _ => TransportMode::Tcp,
    };
    println!("  Transport:            {transport:?}");
    println!();

    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    params.set_transport_mode(transport);
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper
        .endpoint()
        .ok_or_else(|| hyperdb_api::Error::internal("HyperProcess has no endpoint"))?
        .to_string();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers.max(2))
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let mut records: Vec<BenchRecord> = Vec::new();

    // =====================================================================
    //                        INSERT WORKLOADS
    // =====================================================================

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("                           INSERT WORKLOADS");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    // Sync Inserter (HyperBinary, single connection).
    {
        let db = "test_results/suite_sync_inserter.hyper";
        let _ = std::fs::remove_file(db);
        let rec = sync_inserter(&endpoint, db, rows)?;
        println!(
            "  sync  · Inserter                  {:>9} rows · {:.3}s · {}",
            fmt_count(rec.rows),
            rec.elapsed_secs,
            fmt_rate(rec.rows_per_sec())
        );
        records.push(rec);
    }

    // Sync ChunkSender (multi-threaded, single connection).
    {
        let db = "test_results/suite_sync_chunksender.hyper";
        let _ = std::fs::remove_file(db);
        let rec = sync_chunk_sender(&endpoint, db, rows, workers)?;
        println!(
            "  sync  · ChunkSender (multi-thread){:>9} rows · {:.3}s · {}",
            fmt_count(rec.rows),
            rec.elapsed_secs,
            fmt_rate(rec.rows_per_sec())
        );
        records.push(rec);
    }

    // Async AsyncArrowInserter (single connection).
    {
        let db = "test_results/suite_async_arrow.hyper";
        let _ = std::fs::remove_file(db);
        let rec = rt.block_on(async_arrow_single(&endpoint, db, rows))?;
        println!(
            "  async · AsyncArrowInserter        {:>9} rows · {:.3}s · {}",
            fmt_count(rec.rows),
            rec.elapsed_secs,
            fmt_rate(rec.rows_per_sec())
        );
        records.push(rec);
    }

    // Async AsyncArrowInserter × N parallel connections.
    {
        let db = "test_results/suite_async_arrow_parallel.hyper";
        let _ = std::fs::remove_file(db);
        let rec = rt.block_on(async_arrow_parallel(&endpoint, db, rows, workers))?;
        println!(
            "  async · AsyncArrowInserter × {}    {:>9} rows · {:.3}s · {}",
            workers,
            fmt_count(rec.rows),
            rec.elapsed_secs,
            fmt_rate(rec.rows_per_sec())
        );
        records.push(rec);
    }

    // Async spawn_blocking+ChunkSender × N parallel.
    {
        let db = "test_results/suite_async_blocking_cs.hyper";
        let _ = std::fs::remove_file(db);
        let rec = rt.block_on(async_blocking_chunksender_parallel(
            &endpoint, db, rows, workers,
        ))?;
        println!(
            "  async · spawn_blocking+CS × {}     {:>9} rows · {:.3}s · {}",
            workers,
            fmt_count(rec.rows),
            rec.elapsed_secs,
            fmt_rate(rec.rows_per_sec())
        );
        records.push(rec);
    }

    // =====================================================================
    //                        QUERY WORKLOADS
    // =====================================================================
    //
    // All queries run against the single-connection ChunkSender
    // database so the data shape is identical. For parallel queries
    // we additionally need the parallel-Arrow database (one table
    // per worker), so we reuse that.

    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("                           QUERY WORKLOADS");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let sync_query_db = "test_results/suite_sync_chunksender.hyper";

    for shape in [
        QueryShape::FullScan,
        QueryShape::Filtered,
        QueryShape::Aggregation,
    ] {
        let rec = sync_query(&endpoint, sync_query_db, rows, shape)?;
        println!(
            "  sync  · {:<22}   {:>9} rows · {:.3}s · {}",
            shape.label(),
            fmt_count(rec.rows),
            rec.elapsed_secs,
            fmt_rate(rec.rows_per_sec())
        );
        records.push(rec);

        let rec = rt.block_on(async_query_single(&endpoint, sync_query_db, rows, shape))?;
        println!(
            "  async · {:<22}   {:>9} rows · {:.3}s · {}",
            shape.label(),
            fmt_count(rec.rows),
            rec.elapsed_secs,
            fmt_rate(rec.rows_per_sec())
        );
        records.push(rec);

        // Parallel async query against the async-parallel DB (N tables).
        let rec = rt.block_on(async_query_parallel(
            &endpoint,
            "test_results/suite_async_arrow_parallel.hyper",
            rows,
            workers,
            shape,
        ))?;
        println!(
            "  async · {:<18} × {}   {:>9} rows · {:.3}s · {}",
            shape.label(),
            workers,
            fmt_count(rec.rows),
            rec.elapsed_secs,
            fmt_rate(rec.rows_per_sec())
        );
        records.push(rec);
    }

    // =====================================================================
    //                        REPORTING
    // =====================================================================

    drop(rt);
    drop(hyper);

    let md = records_to_markdown(&records);
    let json = records_to_json(&records, &env);

    let md_path = "test_results/benchmark_suite.md";
    let json_path = "test_results/benchmark_suite.json";
    std::fs::write(md_path, &md).ok();
    std::fs::write(json_path, &json).ok();

    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("                       UNIFIED SYNC-vs-ASYNC REPORT");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    println!("{md}");

    println!("Saved markdown → {md_path}");
    println!("Saved JSON     → {json_path}");
    println!("\nTo update BENCHMARK_GUIDE.md for this platform, paste the table");
    println!("above into the appropriate platform section.");
    Ok(())
}

// =============================================================================
// CLI
// =============================================================================

fn parse_args() -> (i64, usize) {
    let args: Vec<String> = env::args().collect();
    let rows = args
        .get(1)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_ROWS_PER_WORKLOAD);
    let workers = args
        .get(2)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_WORKERS)
        .max(1);
    (rows, workers)
}

// =============================================================================
// INSERT: sync Inserter (HyperBinary, single connection)
// =============================================================================

fn make_table_def(name: &str) -> TableDefinition {
    TableDefinition::new(name)
        .add_required_column("id", SqlType::int())
        .add_nullable_column("sensor_id", SqlType::int())
        .add_nullable_column("value", SqlType::double())
        .add_nullable_column("timestamp", SqlType::big_int())
}

fn sync_inserter(endpoint: &str, db_path: &str, rows: i64) -> Result<BenchRecord> {
    let conn = Connection::connect(endpoint, db_path, CreateMode::CreateAndReplace)?;
    conn.execute_command(&format!("CREATE TABLE measurements ({MEASUREMENTS_DDL})"))?;
    let table_def = make_table_def("measurements");

    let start = Instant::now();
    let total = {
        let mut inserter = Inserter::new(&conn, &table_def)?;
        for i in 0..rows {
            let id = gen_id(0, i);
            inserter.add_i32(id)?;
            inserter.add_i32(gen_sensor_id(id))?;
            inserter.add_f64(gen_value(id))?;
            inserter.add_i64(gen_timestamp(id))?;
            inserter.end_row()?;
        }
        inserter.execute()?
    };
    let elapsed = start.elapsed().as_secs_f64();
    conn.close()?;
    Ok(BenchRecord {
        workload: "insert.bulk".to_string(),
        flavor: "sync",
        variant: "Inserter (HyperBinary)".to_string(),
        rows: total,
        bytes: total as usize * BYTES_PER_ROW,
        elapsed_secs: elapsed,
    })
}

// =============================================================================
// INSERT: sync ChunkSender (multi-threaded, single connection)
// =============================================================================

fn sync_chunk_sender(
    endpoint: &str,
    db_path: &str,
    rows: i64,
    num_workers: usize,
) -> Result<BenchRecord> {
    use std::sync::mpsc;
    use std::thread;

    let conn = Connection::connect(endpoint, db_path, CreateMode::CreateAndReplace)?;
    conn.execute_command(&format!("CREATE TABLE measurements ({MEASUREMENTS_DDL})"))?;
    let table_def = make_table_def("measurements");

    let start = Instant::now();
    let sender = ChunkSender::new(&conn, &table_def)?;
    let (tx, rx) = mpsc::channel::<InsertChunk>();

    let rows_per_worker = rows / num_workers as i64;
    let remainder = rows % num_workers as i64;
    let table_def_arc = Arc::new(table_def);

    let handles: Vec<thread::JoinHandle<Result<()>>> = (0..num_workers)
        .map(|w| {
            let tx = tx.clone();
            let td = Arc::clone(&table_def_arc);
            let worker_rows = if w == num_workers - 1 {
                rows_per_worker + remainder
            } else {
                rows_per_worker
            };
            let start_id = w as i64 * rows_per_worker;
            thread::spawn(move || chunk_sender_worker(start_id, worker_rows, &td, tx))
        })
        .collect();
    drop(tx);

    for chunk in &rx {
        sender.send_chunk(chunk)?;
    }
    for h in handles {
        h.join().expect("worker panic")?;
    }
    let total = sender.finish()?;
    let elapsed = start.elapsed().as_secs_f64();
    conn.close()?;

    Ok(BenchRecord {
        workload: "insert.bulk".to_string(),
        flavor: "sync",
        variant: format!("ChunkSender × {num_workers}"),
        rows: total,
        bytes: total as usize * BYTES_PER_ROW,
        elapsed_secs: elapsed,
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "call-site ergonomics: function consumes logically-owned parameters, refactoring signatures is not worth per-site churn"
)]
fn chunk_sender_worker(
    start_id: i64,
    total_rows: i64,
    table_def: &TableDefinition,
    tx: std::sync::mpsc::Sender<InsertChunk>,
) -> Result<()> {
    let mut chunk = InsertChunk::from_table_definition(table_def);
    for i in 0..total_rows {
        let id = gen_id(start_id, i);
        chunk.add_i32(id)?;
        chunk.add_i32(gen_sensor_id(id))?;
        chunk.add_f64(gen_value(id))?;
        chunk.add_i64(gen_timestamp(id))?;
        chunk.end_row()?;
        if chunk.row_count() >= ROWS_PER_BATCH || chunk.should_flush() {
            tx.send(chunk)
                .map_err(|e| hyperdb_api::Error::internal(format!("mpsc send: {e}")))?;
            chunk = InsertChunk::from_table_definition(table_def);
        }
    }
    if !chunk.is_empty() {
        tx.send(chunk)
            .map_err(|e| hyperdb_api::Error::internal(format!("mpsc send: {e}")))?;
    }
    Ok(())
}

// =============================================================================
// INSERT: async AsyncArrowInserter (single connection)
// =============================================================================

struct SinkBuf(Vec<u8>);
impl std::io::Write for SinkBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn arrow_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("sensor_id", DataType::Int32, true),
        Field::new("value", DataType::Float64, true),
        Field::new("timestamp", DataType::Int64, true),
    ]))
}

/// Push `rows` worth of data starting at `start_id` into `inserter`,
/// driving a `StreamWriter` against a locally-owned buffer that we
/// drain batch-by-batch so peak memory stays bounded.
async fn push_rows_via_arrow(
    inserter: &mut AsyncArrowInserter<'_>,
    schema: Arc<Schema>,
    start_id: i64,
    rows: i64,
) -> Result<usize> {
    let mut total_bytes = 0usize;
    let num_batches = (rows as usize).div_ceil(ROWS_PER_BATCH);
    let mut writer = StreamWriter::try_new(SinkBuf(Vec::with_capacity(16 * 1024 * 1024)), &schema)
        .expect("StreamWriter::try_new");

    for b in 0..num_batches {
        let bs = b * ROWS_PER_BATCH;
        let be = (bs + ROWS_PER_BATCH).min(rows as usize);
        let ids: Vec<i32> = (bs..be).map(|i| gen_id(start_id, i as i64)).collect();
        let sensors: Vec<Option<i32>> = ids.iter().map(|id| Some(gen_sensor_id(*id))).collect();
        let values: Vec<Option<f64>> = ids.iter().map(|id| Some(gen_value(*id))).collect();
        let timestamps: Vec<Option<i64>> = ids.iter().map(|id| Some(gen_timestamp(*id))).collect();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int32Array::from(ids)),
                Arc::new(Int32Array::from(sensors)),
                Arc::new(Float64Array::from(values)),
                Arc::new(Int64Array::from(timestamps)),
            ],
        )
        .expect("RecordBatch::try_new");
        writer.write(&batch).expect("writer.write");
        let bytes: Vec<u8> = writer.get_mut().0.split_off(0);
        if !bytes.is_empty() {
            total_bytes += bytes.len();
            inserter.insert_raw(&bytes).await?;
        }
    }
    writer.finish().expect("writer.finish");
    let tail: Vec<u8> = writer.get_mut().0.split_off(0);
    if !tail.is_empty() {
        total_bytes += tail.len();
        inserter.insert_raw(&tail).await?;
    }
    Ok(total_bytes)
}

async fn async_arrow_single(endpoint: &str, db_path: &str, rows: i64) -> Result<BenchRecord> {
    let conn = AsyncConnection::connect(endpoint, db_path, CreateMode::CreateAndReplace).await?;
    conn.execute_command(&format!("CREATE TABLE measurements ({MEASUREMENTS_DDL})"))
        .await?;
    let table_def = make_table_def("measurements");

    let start = Instant::now();
    let mut inserter =
        AsyncArrowInserter::new(&conn, &table_def)?.with_flush_threshold(16 * 1024 * 1024);
    let _bytes = push_rows_via_arrow(&mut inserter, arrow_schema(), 0, rows).await?;
    let total = inserter.execute().await?;
    let elapsed = start.elapsed().as_secs_f64();
    conn.close().await?;
    Ok(BenchRecord {
        workload: "insert.bulk".to_string(),
        flavor: "async",
        variant: "AsyncArrowInserter".to_string(),
        rows: total,
        bytes: total as usize * BYTES_PER_ROW,
        elapsed_secs: elapsed,
    })
}

// =============================================================================
// INSERT: async AsyncArrowInserter × N parallel connections (one table per worker)
// =============================================================================

async fn async_arrow_parallel(
    endpoint: &str,
    db_path: &str,
    rows: i64,
    num_workers: usize,
) -> Result<BenchRecord> {
    // Bootstrap DB + N tables.
    let bootstrap =
        AsyncConnection::connect(endpoint, db_path, CreateMode::CreateAndReplace).await?;
    for w in 0..num_workers {
        bootstrap
            .execute_command(&format!(
                "CREATE TABLE measurements_{w} ({MEASUREMENTS_DDL})"
            ))
            .await?;
    }
    bootstrap.close().await?;

    let per_worker = rows / num_workers as i64;
    let total_rows = per_worker * num_workers as i64;
    let start = Instant::now();

    let mut tasks = Vec::with_capacity(num_workers);
    for w in 0..num_workers {
        let endpoint = endpoint.to_string();
        let db_path = db_path.to_string();
        tasks.push(tokio::spawn(async move {
            let table = format!("measurements_{w}");
            let td = make_table_def(&table);
            let conn =
                AsyncConnection::connect(&endpoint, &db_path, CreateMode::DoNotCreate).await?;
            let mut inserter =
                AsyncArrowInserter::new(&conn, &td)?.with_flush_threshold(16 * 1024 * 1024);
            let start_id = w as i64 * per_worker;
            let _bytes =
                push_rows_via_arrow(&mut inserter, arrow_schema(), start_id, per_worker).await?;
            let rows = inserter.execute().await?;
            conn.close().await?;
            Ok::<u64, hyperdb_api::Error>(rows)
        }));
    }

    let mut total: u64 = 0;
    for t in tasks {
        total += t
            .await
            .map_err(|e| hyperdb_api::Error::internal(format!("join: {e}")))??;
    }
    let elapsed = start.elapsed().as_secs_f64();

    Ok(BenchRecord {
        workload: "insert.bulk".to_string(),
        flavor: "async",
        variant: format!("AsyncArrowInserter × {num_workers}"),
        rows: total,
        bytes: total_rows as usize * BYTES_PER_ROW,
        elapsed_secs: elapsed,
    })
}

// =============================================================================
// INSERT: async spawn_blocking + sync ChunkSender × N parallel
// =============================================================================

async fn async_blocking_chunksender_parallel(
    endpoint: &str,
    db_path: &str,
    rows: i64,
    num_workers: usize,
) -> Result<BenchRecord> {
    // Bootstrap (sync in a blocking task).
    {
        let endpoint = endpoint.to_string();
        let db_path = db_path.to_string();
        let n = num_workers;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = Connection::connect(&endpoint, &db_path, CreateMode::CreateAndReplace)?;
            for w in 0..n {
                conn.execute_command(&format!(
                    "CREATE TABLE measurements_{w} ({MEASUREMENTS_DDL})"
                ))?;
            }
            conn.close()
        })
        .await
        .map_err(|e| hyperdb_api::Error::internal(format!("bootstrap join: {e}")))??;
    }

    let per_worker = rows / num_workers as i64;
    let total_rows = per_worker * num_workers as i64;
    let start = Instant::now();

    let mut tasks = Vec::with_capacity(num_workers);
    for w in 0..num_workers {
        let endpoint = endpoint.to_string();
        let db_path = db_path.to_string();
        tasks.push(tokio::task::spawn_blocking(move || -> Result<u64> {
            let table = format!("measurements_{w}");
            let td = make_table_def(&table);
            let conn = Connection::connect(&endpoint, &db_path, CreateMode::DoNotCreate)?;
            let sender = ChunkSender::new(&conn, &td)?;
            let start_id = w as i64 * per_worker;

            let mut chunk = InsertChunk::from_table_definition(&td);
            for i in 0..per_worker {
                let id = gen_id(start_id, i);
                chunk.add_i32(id)?;
                chunk.add_i32(gen_sensor_id(id))?;
                chunk.add_f64(gen_value(id))?;
                chunk.add_i64(gen_timestamp(id))?;
                chunk.end_row()?;
                if chunk.row_count() >= ROWS_PER_BATCH || chunk.should_flush() {
                    sender.send_chunk(chunk)?;
                    chunk = InsertChunk::from_table_definition(&td);
                }
            }
            if !chunk.is_empty() {
                sender.send_chunk(chunk)?;
            }
            let rows = sender.finish()?;
            conn.close()?;
            Ok(rows)
        }));
    }

    let mut total: u64 = 0;
    for t in tasks {
        total += t
            .await
            .map_err(|e| hyperdb_api::Error::internal(format!("blocking join: {e}")))??;
    }
    let elapsed = start.elapsed().as_secs_f64();

    Ok(BenchRecord {
        workload: "insert.bulk".to_string(),
        flavor: "async",
        variant: format!("spawn_blocking+ChunkSender × {num_workers}"),
        rows: total,
        bytes: total_rows as usize * BYTES_PER_ROW,
        elapsed_secs: elapsed,
    })
}

// =============================================================================
// QUERY: shapes + sync/async variants
// =============================================================================

#[derive(Clone, Copy, Debug)]
enum QueryShape {
    FullScan,
    Filtered,
    Aggregation,
}

impl QueryShape {
    fn label(self) -> &'static str {
        match self {
            QueryShape::FullScan => "query.full_scan",
            QueryShape::Filtered => "query.filtered",
            QueryShape::Aggregation => "query.aggregation",
        }
    }
    fn sql(self, table: &str) -> String {
        match self {
            QueryShape::FullScan => {
                format!("SELECT id, sensor_id, value, timestamp FROM {table}")
            }
            QueryShape::Filtered => {
                format!("SELECT id, value FROM {table} WHERE sensor_id = 5")
            }
            QueryShape::Aggregation => {
                format!("SELECT sensor_id, COUNT(*), SUM(value) FROM {table} GROUP BY sensor_id")
            }
        }
    }
    fn bytes_per_row(self) -> usize {
        match self {
            QueryShape::FullScan => 24,
            QueryShape::Filtered => 12,
            QueryShape::Aggregation => 24,
        }
    }
}

fn sync_query(endpoint: &str, db_path: &str, rows: i64, shape: QueryShape) -> Result<BenchRecord> {
    let conn = Connection::connect(endpoint, db_path, CreateMode::DoNotCreate)?;
    let sql = shape.sql("measurements");
    let start = Instant::now();
    let mut rs = conn.execute_query(&sql)?;
    let mut count: u64 = 0;
    let mut checksum: f64 = 0.0;
    while let Some(chunk) = rs.next_chunk()? {
        for row in &chunk {
            count += 1;
            let idx = match shape {
                QueryShape::FullScan => 2,
                QueryShape::Filtered => 1,
                QueryShape::Aggregation => 2,
            };
            if let Some(v) = row.get::<f64>(idx) {
                checksum += v;
            }
        }
    }
    drop(rs);
    let elapsed = start.elapsed().as_secs_f64();
    let _ = (checksum, rows);
    conn.close()?;

    Ok(BenchRecord {
        workload: shape.label().to_string(),
        flavor: "sync",
        variant: "single connection".to_string(),
        rows: count,
        bytes: count as usize * shape.bytes_per_row(),
        elapsed_secs: elapsed,
    })
}

async fn async_query_single(
    endpoint: &str,
    db_path: &str,
    rows: i64,
    shape: QueryShape,
) -> Result<BenchRecord> {
    let conn = AsyncConnection::connect(endpoint, db_path, CreateMode::DoNotCreate).await?;
    let sql = shape.sql("measurements");
    let start = Instant::now();
    let mut rs = conn.execute_query(&sql).await?;
    let mut count: u64 = 0;
    let mut checksum: f64 = 0.0;
    while let Some(chunk) = rs.next_chunk().await? {
        for row in &chunk {
            count += 1;
            let idx = match shape {
                QueryShape::FullScan => 2,
                QueryShape::Filtered => 1,
                QueryShape::Aggregation => 2,
            };
            if let Some(v) = row.get::<f64>(idx) {
                checksum += v;
            }
        }
    }
    drop(rs);
    let elapsed = start.elapsed().as_secs_f64();
    let _ = (checksum, rows);
    conn.close().await?;

    Ok(BenchRecord {
        workload: shape.label().to_string(),
        flavor: "async",
        variant: "single connection".to_string(),
        rows: count,
        bytes: count as usize * shape.bytes_per_row(),
        elapsed_secs: elapsed,
    })
}

async fn async_query_parallel(
    endpoint: &str,
    db_path: &str,
    _rows: i64,
    num_workers: usize,
    shape: QueryShape,
) -> Result<BenchRecord> {
    let start = Instant::now();
    let mut tasks = Vec::with_capacity(num_workers);
    for w in 0..num_workers {
        let endpoint = endpoint.to_string();
        let db_path = db_path.to_string();
        tasks.push(tokio::spawn(async move {
            let table = format!("measurements_{w}");
            let sql = shape.sql(&table);
            let conn =
                AsyncConnection::connect(&endpoint, &db_path, CreateMode::DoNotCreate).await?;
            let mut rs = conn.execute_query(&sql).await?;
            let mut count: u64 = 0;
            let mut checksum: f64 = 0.0;
            while let Some(chunk) = rs.next_chunk().await? {
                for row in &chunk {
                    count += 1;
                    let idx = match shape {
                        QueryShape::FullScan => 2,
                        QueryShape::Filtered => 1,
                        QueryShape::Aggregation => 2,
                    };
                    if let Some(v) = row.get::<f64>(idx) {
                        checksum += v;
                    }
                }
            }
            drop(rs);
            conn.close().await?;
            let _ = checksum;
            Ok::<u64, hyperdb_api::Error>(count)
        }));
    }

    let mut total: u64 = 0;
    for t in tasks {
        total += t
            .await
            .map_err(|e| hyperdb_api::Error::internal(format!("query join: {e}")))??;
    }
    let elapsed = start.elapsed().as_secs_f64();

    Ok(BenchRecord {
        workload: shape.label().to_string(),
        flavor: "async",
        variant: format!("{num_workers} parallel connections"),
        rows: total,
        bytes: total as usize * shape.bytes_per_row(),
        elapsed_secs: elapsed,
    })
}
