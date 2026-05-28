// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async parallel-connection benchmark.
//!
//! Demonstrates what multiple async TCP connections can do when each
//! owns its own table on a shared `HyperProcess`:
//!
//! - Workload 1: parallel insert via [`AsyncArrowInserter`]
//!   ("truly async" — all IO on the tokio runtime).
//! - Workload 2: parallel insert via `tokio::task::spawn_blocking` +
//!   sync [`ChunkSender`] ("async-wrapped-sync" — bridges the best-
//!   in-class sync bulk path into an async program).
//! - Workload 3: parallel streaming queries in three shapes (full
//!   scan / filtered / GROUP BY aggregation) via
//!   [`AsyncConnection::execute_query`] + `AsyncRowset::next_chunk`.
//!
//! Each worker works on its **own table** (`measurements_0`,
//! `measurements_1`, …) so there's no server-side contention between
//! connections. Default is 4 workers × (`ROW_COUNT` / 4) rows each;
//! override with CLI args.
//!
//! Run with:
//! ```sh
//! HYPERD_PATH=~/dev/bin/hyperd \
//!   cargo run -p hyperdb-api --release --example async_parallel_benchmark
//! # or at a custom total:
//! HYPERD_PATH=~/dev/bin/hyperd \
//!   cargo run -p hyperdb-api --release --example async_parallel_benchmark -- 20000000
//! # or with a custom worker count:
//! HYPERD_PATH=~/dev/bin/hyperd \
//!   cargo run -p hyperdb-api --release --example async_parallel_benchmark -- 20000000 8
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
    InsertChunk, Parameters, Result, SqlType, TableDefinition,
};

use common::{fmt_count, fmt_rate, BYTES_PER_ROW};

/// Await all `handles`, converting join errors into `hyperdb_api::Error`
/// and collecting successful results. Replaces `futures::try_join_all`
/// so the bench needs no extra dev-dep.
async fn try_join_all_tasks<T>(handles: Vec<tokio::task::JoinHandle<Result<T>>>) -> Result<Vec<T>> {
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        let v = h
            .await
            .map_err(|e| hyperdb_api::Error::internal(format!("task join error: {e}")))??;
        out.push(v);
    }
    Ok(out)
}

/// Arrow `StreamWriter` sink that owns a growable `Vec<u8>` we can
/// drain between batches. `StreamWriter` itself holds a `&mut` to this
/// type, so ownership of the buffer stays here.
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

const DEFAULT_TOTAL_ROWS: i64 = 100_000_000;
const DEFAULT_WORKERS: usize = 4;
const ROWS_PER_BATCH: usize = 100_000;
// BYTES_PER_ROW comes from common.rs (24 bytes: i32 + i32 + f64 + i64).

fn main() -> Result<()> {
    let (total_rows, num_workers) = parse_args();

    let per_worker = total_rows / num_workers as i64;
    let total_rows = per_worker * num_workers as i64; // round to clean split

    println!("=== Async Parallel Connection Benchmark ===\n");
    println!("Configuration:");
    println!("  Workers:          {num_workers}");
    println!("  Rows per worker:  {}", fmt_count(per_worker as u64));
    println!("  Total rows:       {}", fmt_count(total_rows as u64));
    println!(
        "  Schema:           measurements(id INT, sensor_id INT, value DOUBLE, timestamp BIGINT)"
    );
    println!("  Bytes per row:    {BYTES_PER_ROW}");
    println!();

    std::fs::create_dir_all("test_results").ok();

    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper
        .endpoint()
        .ok_or_else(|| hyperdb_api::Error::internal("HyperProcess has no TCP endpoint"))?
        .to_string();

    // Multi-thread runtime so tokio can run N async inserts truly in parallel.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_workers.max(2))
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    // -------------------------------------------------------------
    // Workload 1 — AsyncArrowInserter per worker, all tasks on tokio.
    // -------------------------------------------------------------
    let arrow_db = "test_results/async_parallel_arrow.hyper";
    let arrow_result = rt.block_on(run_parallel_arrow_insert(
        &endpoint,
        arrow_db,
        num_workers,
        per_worker,
    ))?;

    // -------------------------------------------------------------
    // Workload 2 — spawn_blocking + sync ChunkSender per worker.
    // -------------------------------------------------------------
    let chunk_db = "test_results/async_parallel_chunksender.hyper";
    let chunk_result = rt.block_on(run_parallel_chunk_sender(
        &endpoint,
        chunk_db,
        num_workers,
        per_worker,
    ))?;

    print_insert_table(total_rows, &arrow_result, &chunk_result);

    // -------------------------------------------------------------
    // Workload 3 — parallel streaming queries against the
    // ChunkSender-populated tables (they have real data and each
    // worker reads its own table).
    // -------------------------------------------------------------
    let scan_result = rt.block_on(run_parallel_query(
        &endpoint,
        chunk_db,
        num_workers,
        QueryShape::FullScan,
    ))?;
    let filter_result = rt.block_on(run_parallel_query(
        &endpoint,
        chunk_db,
        num_workers,
        QueryShape::Filtered,
    ))?;
    let agg_result = rt.block_on(run_parallel_query(
        &endpoint,
        chunk_db,
        num_workers,
        QueryShape::Aggregation,
    ))?;

    print_query_table(
        total_rows,
        num_workers,
        &scan_result,
        &filter_result,
        &agg_result,
    );

    // Clean up the runtime before HyperProcess drops.
    drop(rt);

    println!("\nBenchmark completed!");
    Ok(())
}

// =============================================================================
// CLI parsing
// =============================================================================

fn parse_args() -> (i64, usize) {
    let args: Vec<String> = env::args().collect();
    let total = args
        .get(1)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_TOTAL_ROWS);
    let workers = args
        .get(2)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_WORKERS)
        .max(1);
    (total, workers)
}

// =============================================================================
// Workload 1 — parallel AsyncArrowInserter
// =============================================================================

#[derive(Clone)]
struct WorkerResult {
    worker_time_secs: f64,
    rows: u64,
    bytes: usize,
}

struct BenchTotals {
    wall_secs: f64,
    workers: Vec<WorkerResult>,
}

impl BenchTotals {
    fn total_rows(&self) -> u64 {
        self.workers.iter().map(|w| w.rows).sum()
    }
    fn total_bytes(&self) -> usize {
        self.workers.iter().map(|w| w.bytes).sum()
    }
    fn agg_rows_per_sec(&self) -> f64 {
        self.total_rows() as f64 / self.wall_secs
    }
    fn agg_mb_per_sec(&self) -> f64 {
        (self.total_bytes() as f64) / (1024.0 * 1024.0) / self.wall_secs
    }
    /// Ratio of summed per-worker time to wall-clock time. ~N means
    /// near-perfect parallelism; 1.0 means fully serial.
    fn parallelism(&self) -> f64 {
        let sum: f64 = self.workers.iter().map(|w| w.worker_time_secs).sum();
        if self.wall_secs <= 0.0 {
            0.0
        } else {
            sum / self.wall_secs
        }
    }
}

async fn run_parallel_arrow_insert(
    endpoint: &str,
    db_path: &str,
    num_workers: usize,
    per_worker: i64,
) -> Result<BenchTotals> {
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("        WORKLOAD 1: Parallel AsyncArrowInserter ({num_workers} connections)");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    // Bootstrap connection: creates the database and all N tables.
    let bootstrap =
        AsyncConnection::connect(endpoint, db_path, CreateMode::CreateAndReplace).await?;
    for k in 0..num_workers {
        bootstrap
            .execute_command(&format!(
                "CREATE TABLE measurements_{k} (id INT NOT NULL, sensor_id INT, \
                 value DOUBLE PRECISION, timestamp BIGINT)"
            ))
            .await?;
    }
    bootstrap.close().await?;

    let wall_start = Instant::now();
    let endpoint = endpoint.to_string();
    let db_path = db_path.to_string();

    let mut tasks = Vec::with_capacity(num_workers);
    for worker_id in 0..num_workers {
        let endpoint = endpoint.clone();
        let db_path = db_path.clone();
        tasks.push(tokio::spawn(async move {
            arrow_worker(&endpoint, &db_path, worker_id, per_worker).await
        }));
    }

    let results: Vec<WorkerResult> = try_join_all_tasks(tasks).await?;

    let wall = wall_start.elapsed();
    let totals = BenchTotals {
        wall_secs: wall.as_secs_f64(),
        workers: results,
    };

    println!(
        "  Wall: {:.3}s · {} rows · {:.2} MB/s · parallelism {:.2}x",
        totals.wall_secs,
        fmt_count(totals.total_rows()),
        totals.agg_mb_per_sec(),
        totals.parallelism()
    );
    Ok(totals)
}

async fn arrow_worker(
    endpoint: &str,
    db_path: &str,
    worker_id: usize,
    per_worker: i64,
) -> Result<WorkerResult> {
    let table_name = format!("measurements_{worker_id}");
    let table_def = TableDefinition::new(&table_name)
        .add_required_column("id", SqlType::int())
        .add_nullable_column("sensor_id", SqlType::int())
        .add_nullable_column("value", SqlType::double())
        .add_nullable_column("timestamp", SqlType::big_int());

    let conn = AsyncConnection::connect(endpoint, db_path, CreateMode::DoNotCreate).await?;

    // One Arrow IPC schema per task — encoded once and kept live for
    // the whole insert so batches 2..N skip the header.
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("sensor_id", DataType::Int32, true),
        Field::new("value", DataType::Float64, true),
        Field::new("timestamp", DataType::Int64, true),
    ]));

    let start = Instant::now();
    let mut inserter =
        AsyncArrowInserter::new(&conn, &table_def)?.with_flush_threshold(16 * 1024 * 1024);

    let start_id = worker_id as i64 * per_worker;
    let num_batches = (per_worker as usize).div_ceil(ROWS_PER_BATCH);
    let mut total_bytes = 0usize;

    // `SinkBuf` owns a `Vec<u8>` that `StreamWriter` writes into; after
    // each batch we drain the buffer and hand those bytes to the
    // inserter. `SinkBuf` is `Send`, so this whole future is `Send`
    // across the tokio multi-thread runtime (unlike
    // `Rc<RefCell<Vec<u8>>>` in the arrow_batching_benchmark, which
    // uses a single-thread runtime).
    let mut writer = StreamWriter::try_new(SinkBuf(Vec::with_capacity(16 * 1024 * 1024)), &schema)
        .expect("StreamWriter::try_new");

    for batch_idx in 0..num_batches {
        let batch_start = batch_idx * ROWS_PER_BATCH;
        let batch_end = (batch_start + ROWS_PER_BATCH).min(per_worker as usize);
        let len = batch_end - batch_start;

        let ids: Vec<i32> = (batch_start..batch_end)
            .map(|i| (start_id + i as i64) as i32)
            .collect();
        let sensor_ids: Vec<Option<i32>> = (batch_start..batch_end)
            .map(|i| Some(((start_id + i as i64) % 10) as i32))
            .collect();
        let values: Vec<Option<f64>> = (batch_start..batch_end)
            .map(|i| Some((start_id + i as i64) as f64 * 0.1))
            .collect();
        let timestamps: Vec<Option<i64>> = (batch_start..batch_end)
            .map(|i| Some(1_700_000_000_000i64 + (start_id + i as i64) * 1000))
            .collect();

        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int32Array::from(ids)),
                Arc::new(Int32Array::from(sensor_ids)),
                Arc::new(Float64Array::from(values)),
                Arc::new(Int64Array::from(timestamps)),
            ],
        )
        .expect("RecordBatch::try_new");
        let _ = len;
        writer.write(&batch).expect("StreamWriter::write");

        // Drain the sink after each batch so we stream IPC bytes to
        // the server instead of buffering the whole worker in memory.
        // Keeps peak per-worker memory at ~one batch (~4 MB).
        let bytes: Vec<u8> = writer.get_mut().0.split_off(0);
        if !bytes.is_empty() {
            total_bytes += bytes.len();
            inserter.insert_raw(&bytes).await?;
        }
    }

    // Emit the final end-of-stream marker and drain any tail bytes.
    writer.finish().expect("StreamWriter::finish");
    let tail: Vec<u8> = writer.get_mut().0.split_off(0);
    if !tail.is_empty() {
        total_bytes += tail.len();
        inserter.insert_raw(&tail).await?;
    }

    let rows = inserter.execute().await?;
    conn.close().await?;
    let worker_time = start.elapsed();

    println!(
        "  [arrow worker {}] {:>9} rows · {:.3}s · {:>6.1} MB/s",
        worker_id,
        fmt_count(rows),
        worker_time.as_secs_f64(),
        (total_bytes as f64) / (1024.0 * 1024.0) / worker_time.as_secs_f64()
    );

    Ok(WorkerResult {
        worker_time_secs: worker_time.as_secs_f64(),
        rows,
        bytes: total_bytes,
    })
}

// =============================================================================
// Workload 2 — parallel spawn_blocking + sync ChunkSender
// =============================================================================

async fn run_parallel_chunk_sender(
    endpoint: &str,
    db_path: &str,
    num_workers: usize,
    per_worker: i64,
) -> Result<BenchTotals> {
    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("     WORKLOAD 2: spawn_blocking + sync ChunkSender ({num_workers} connections)");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    // Bootstrap: create DB + tables via sync connection for simplicity.
    {
        let endpoint = endpoint.to_string();
        let db_path = db_path.to_string();
        let n = num_workers;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = Connection::connect(&endpoint, &db_path, CreateMode::CreateAndReplace)?;
            for k in 0..n {
                conn.execute_command(&format!(
                    "CREATE TABLE measurements_{k} (id INT NOT NULL, sensor_id INT, \
                     value DOUBLE PRECISION, timestamp BIGINT)"
                ))?;
            }
            conn.close()
        })
        .await
        .map_err(|e| hyperdb_api::Error::internal(format!("bootstrap join error: {e}")))??;
    }

    let wall_start = Instant::now();
    let mut tasks = Vec::with_capacity(num_workers);
    for worker_id in 0..num_workers {
        let endpoint = endpoint.to_string();
        let db_path = db_path.to_string();
        tasks.push(tokio::task::spawn_blocking(
            move || -> Result<WorkerResult> {
                chunk_sender_worker(&endpoint, &db_path, worker_id, per_worker)
            },
        ));
    }

    let results: Vec<WorkerResult> = try_join_all_tasks(tasks).await?;

    let wall = wall_start.elapsed();
    let totals = BenchTotals {
        wall_secs: wall.as_secs_f64(),
        workers: results,
    };

    println!(
        "  Wall: {:.3}s · {} rows · {:.2} MB/s · parallelism {:.2}x",
        totals.wall_secs,
        fmt_count(totals.total_rows()),
        totals.agg_mb_per_sec(),
        totals.parallelism()
    );
    Ok(totals)
}

fn chunk_sender_worker(
    endpoint: &str,
    db_path: &str,
    worker_id: usize,
    per_worker: i64,
) -> Result<WorkerResult> {
    let table_name = format!("measurements_{worker_id}");
    let table_def = TableDefinition::new(&table_name)
        .add_required_column("id", SqlType::int())
        .add_nullable_column("sensor_id", SqlType::int())
        .add_nullable_column("value", SqlType::double())
        .add_nullable_column("timestamp", SqlType::big_int());

    let conn = Connection::connect(endpoint, db_path, CreateMode::DoNotCreate)?;

    let start = Instant::now();
    let sender = ChunkSender::new(&conn, &table_def)?;

    let start_id = worker_id as i64 * per_worker;
    let mut chunk = InsertChunk::from_table_definition(&table_def);
    let mut rows_sent = 0u64;

    for i in 0..per_worker {
        let id = (start_id + i) as i32;
        let sensor_id = id % 10;
        let value = f64::from(id) * 0.1;
        let timestamp = 1_700_000_000_000i64 + i64::from(id) * 1000;

        chunk.add_i32(id)?;
        chunk.add_i32(sensor_id)?;
        chunk.add_f64(value)?;
        chunk.add_i64(timestamp)?;
        chunk.end_row()?;

        if chunk.row_count() >= ROWS_PER_BATCH || chunk.should_flush() {
            sender.send_chunk(chunk)?;
            chunk = InsertChunk::from_table_definition(&table_def);
        }
        rows_sent += 1;
    }

    if !chunk.is_empty() {
        sender.send_chunk(chunk)?;
    }

    let rows = sender.finish()?;
    conn.close()?;
    let worker_time = start.elapsed();
    let total_bytes = rows_sent as usize * BYTES_PER_ROW;

    println!(
        "  [chunk worker {}] {:>9} rows · {:.3}s · {:>6.1} MB/s",
        worker_id,
        fmt_count(rows),
        worker_time.as_secs_f64(),
        (total_bytes as f64) / (1024.0 * 1024.0) / worker_time.as_secs_f64()
    );

    Ok(WorkerResult {
        worker_time_secs: worker_time.as_secs_f64(),
        rows,
        bytes: total_bytes,
    })
}

// =============================================================================
// Workload 3 — parallel streaming queries (3 shapes)
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
            QueryShape::FullScan => "full scan",
            QueryShape::Filtered => "filtered",
            QueryShape::Aggregation => "aggregation",
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
    /// Approximate bytes per returned row, used for MB/s reporting.
    fn bytes_per_row(self) -> usize {
        match self {
            QueryShape::FullScan => 24,    // i32 + i32 + f64 + i64
            QueryShape::Filtered => 12,    // i32 + f64
            QueryShape::Aggregation => 24, // ~10 group rows, not perf-sensitive
        }
    }
}

async fn run_parallel_query(
    endpoint: &str,
    db_path: &str,
    num_workers: usize,
    shape: QueryShape,
) -> Result<BenchTotals> {
    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!(
        "  WORKLOAD 3: Parallel streaming query — {} ({} connections)",
        shape.label(),
        num_workers
    );
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let wall_start = Instant::now();
    let mut tasks = Vec::with_capacity(num_workers);
    for worker_id in 0..num_workers {
        let endpoint = endpoint.to_string();
        let db_path = db_path.to_string();
        tasks.push(tokio::spawn(async move {
            query_worker(&endpoint, &db_path, worker_id, shape).await
        }));
    }

    let results: Vec<WorkerResult> = try_join_all_tasks(tasks).await?;

    let wall = wall_start.elapsed();
    let totals = BenchTotals {
        wall_secs: wall.as_secs_f64(),
        workers: results,
    };

    println!(
        "  Wall: {:.3}s · {} rows · {:.2} MB/s · parallelism {:.2}x",
        totals.wall_secs,
        fmt_count(totals.total_rows()),
        totals.agg_mb_per_sec(),
        totals.parallelism()
    );
    Ok(totals)
}

async fn query_worker(
    endpoint: &str,
    db_path: &str,
    worker_id: usize,
    shape: QueryShape,
) -> Result<WorkerResult> {
    let table = format!("measurements_{worker_id}");
    let sql = shape.sql(&table);

    let conn = AsyncConnection::connect(endpoint, db_path, CreateMode::DoNotCreate).await?;
    let start = Instant::now();

    let mut rs = conn.execute_query(&sql).await?;
    let mut rows = 0u64;
    // Touch value/sum_id so the optimizer can't elide work.
    let mut checksum: f64 = 0.0;
    while let Some(chunk) = rs.next_chunk().await? {
        for row in &chunk {
            rows += 1;
            match shape {
                QueryShape::FullScan => {
                    if let Some(v) = row.get::<f64>(2) {
                        checksum += v;
                    }
                }
                QueryShape::Filtered => {
                    if let Some(v) = row.get::<f64>(1) {
                        checksum += v;
                    }
                }
                QueryShape::Aggregation => {
                    if let Some(v) = row.get::<f64>(2) {
                        checksum += v;
                    }
                }
            }
        }
    }
    drop(rs);
    conn.close().await?;
    let worker_time = start.elapsed();
    let bytes = rows as usize * shape.bytes_per_row();

    println!(
        "  [{} worker {}] {:>9} rows · {:.3}s · {:>6.1} MB/s · checksum≈{:.1}",
        shape.label(),
        worker_id,
        fmt_count(rows),
        worker_time.as_secs_f64(),
        (bytes as f64) / (1024.0 * 1024.0) / worker_time.as_secs_f64().max(1e-9),
        checksum
    );

    Ok(WorkerResult {
        worker_time_secs: worker_time.as_secs_f64(),
        rows,
        bytes,
    })
}

// =============================================================================
// Reporting
// =============================================================================

fn print_insert_table(total_rows: i64, arrow: &BenchTotals, chunk: &BenchTotals) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!(
        "║        PARALLEL ASYNC INSERT — {} workers × {} rows (total {})",
        arrow.workers.len(),
        fmt_count((total_rows as usize / arrow.workers.len()) as u64),
        fmt_count(total_rows as u64)
    );
    println!("╠══════════════════════════════════════════════════════════════════════════════╣");
    println!("║ Path                       │ Wall (s) │   Rows/sec   │   MB/sec   │ Parallel ║");
    println!("╠════════════════════════════╪══════════╪══════════════╪════════════╪══════════╣");
    print_insert_row("AsyncArrowInserter         ", arrow);
    print_insert_row("spawn_blocking+ChunkSender ", chunk);
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
}

fn print_insert_row(label: &str, t: &BenchTotals) {
    println!(
        "║ {} │ {:>8.3} │ {:>12} │ {:>10.1} │ {:>7.2}x ║",
        label,
        t.wall_secs,
        fmt_rate(t.agg_rows_per_sec()),
        t.agg_mb_per_sec(),
        t.parallelism()
    );
}

fn print_query_table(
    total_rows: i64,
    num_workers: usize,
    scan: &BenchTotals,
    filter: &BenchTotals,
    agg: &BenchTotals,
) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!(
        "║     PARALLEL ASYNC QUERY — {} workers × {} rows/table (total {})",
        num_workers,
        fmt_count((total_rows as usize / num_workers) as u64),
        fmt_count(total_rows as u64)
    );
    println!("╠══════════════════════════════════════════════════════════════════════════════╣");
    println!("║ Query shape                │ Wall (s) │   Rows/sec   │   MB/sec   │ Parallel ║");
    println!("╠════════════════════════════╪══════════╪══════════════╪════════════╪══════════╣");
    print_insert_row("full scan                  ", scan);
    print_insert_row("filtered (sensor_id = 5)   ", filter);
    print_insert_row("GROUP BY sensor_id         ", agg);
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
}

// Formatting helpers (fmt_count, fmt_rate) come from `common.rs`.
