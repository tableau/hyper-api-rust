// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Performance benchmarks for Hyper API
//!
//! Includes:
//! - Bulk insert benchmark (via Inserter - single-threaded)
//! - Bulk insert benchmark (via InsertChunk/ChunkSender - multi-threaded)
//! - Query benchmarks (full scan, filtered, aggregation)
//! - System resource monitoring (CPU, memory) during benchmarks
//!
//! Run with: cargo run -p hyperdb-api --example benchmark [`ROW_COUNT`]
//! Or release: cargo run -p hyperdb-api --release --example benchmark [`ROW_COUNT`]
//!
//! Examples:
//!   cargo run -p hyperdb-api --release --example benchmark           # Default 10M rows
//!   cargo run -p hyperdb-api --release --example benchmark 100000000 # 100M rows

// Benchmark harness: intentional wide→narrow conversions for row-count display,
// throughput math, and indexing with bounds the benchmark itself enforces.
// Blanket-allowing here keeps per-site ceremony out of perf code while leaving
// the deny-level rules intact for production.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    reason = "benchmark harness: counts/indices narrow by bench-enforced invariants, throughput math needs f64"
)]

#[path = "common.rs"]
mod common;

use hyperdb_api::{
    Catalog, ChunkSender, Connection, CreateMode, HyperProcess, InsertChunk, Inserter, Result,
    SqlType, TableDefinition, TransportMode,
};
use std::env;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use common::{ResourceMonitor, ResourceStats, SAMPLE_INTERVAL_MS};

// Default 10M rows for comparison with C++ benchmark
const DEFAULT_ROW_COUNT: i64 = 10_000_000;

fn get_row_count() -> i64 {
    env::args()
        .nth(1)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_ROW_COUNT)
}

/// Formats a row count with appropriate suffix (K, M, B).
fn format_row_count(count: i64) -> String {
    if count >= 1_000_000_000 {
        format!("{}B", count / 1_000_000_000)
    } else if count >= 1_000_000 {
        format!("{}M", count / 1_000_000)
    } else if count >= 1_000 {
        format!("{}K", count / 1_000)
    } else {
        format!("{count}")
    }
}

/// Formats a count with decimal suffix (K, M, B) - matches grpc benchmark style.
fn format_count(count: u64) -> String {
    if count >= 1_000_000_000 {
        format!("{:.1}B", count as f64 / 1_000_000_000.0)
    } else if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        format!("{count}")
    }
}

/// Formats a byte size with appropriate suffix (B, KB, MB, GB).
fn format_size(bytes: usize) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.2} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.2} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

/// Calculates approximate bytes per row for the benchmark table.
/// Table structure: id (i32), `sensor_id` (i32), value (f64), timestamp (i64)
/// = 4 + 4 + 8 + 8 = 24 bytes per row (approximate, excluding overhead)
fn bytes_per_row() -> usize {
    24
}

/// Calculates MB/sec from bytes and elapsed time.
fn mb_per_sec(bytes: f64, elapsed_secs: f64) -> f64 {
    if elapsed_secs <= 0.0 {
        return 0.0;
    }
    bytes / elapsed_secs / (1024.0 * 1024.0)
}

/// Result of a benchmark run including timing and resource stats.
#[derive(Debug)]
struct BenchmarkResult {
    /// Elapsed wall-clock time for the benchmarked operation, in seconds.
    elapsed_secs: f64,
    /// Throughput: rows processed per second.
    rows_per_sec: f64,
    /// CPU / memory samples captured during the benchmark run.
    resource_stats: ResourceStats,
}

/// Result of a query benchmark with timing and throughput metrics.
#[derive(Debug, Clone)]
struct QueryBenchmarkResult {
    query_name: String,
    row_count: u64,
    data_size_bytes: usize,
    elapsed_secs: f64,
    rows_per_sec: f64,
    mb_per_sec: f64,
}

impl QueryBenchmarkResult {
    fn new(
        query_name: String,
        row_count: u64,
        data_size_bytes: usize,
        elapsed: std::time::Duration,
    ) -> Self {
        let elapsed_secs = elapsed.as_secs_f64();
        let rows_per_sec = row_count as f64 / elapsed_secs;
        let mb_per_sec = (data_size_bytes as f64 / 1_000_000.0) / elapsed_secs;

        QueryBenchmarkResult {
            query_name,
            row_count,
            data_size_bytes,
            elapsed_secs,
            rows_per_sec,
            mb_per_sec,
        }
    }
}

// ============================================================================
// Table Formatting Functions
// ============================================================================

fn print_header(title: &str) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║ {title:^76} ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();
}

fn print_section(title: &str) {
    println!();
    println!("┌──────────────────────────────────────────────────────────────────────────────┐");
    println!("│ {}{}│", title, " ".repeat(76 - title.len()));
    println!("└──────────────────────────────────────────────────────────────────────────────┘");
}

fn print_table_header() {
    println!();
    println!("┌────────────┬────────────┬────────────┬────────────┬──────────────┬──────────────┐");
    println!(
        "│ {:>10} │ {:>10} │ {:>10} │ {:>10} │ {:>12} │ {:>12} │",
        "Query", "Rows", "Data Size", "Time (s)", "Rows/sec", "MB/sec"
    );
    println!("├────────────┼────────────┼────────────┼────────────┼──────────────┼──────────────┤");
}

fn print_table_row(result: &QueryBenchmarkResult) {
    println!(
        "│ {:>10} │ {:>10} │ {:>10} │ {:>10.2} │ {:>12} │ {:>12.2} │",
        result.query_name,
        format_count(result.row_count),
        format_size(result.data_size_bytes),
        result.elapsed_secs,
        format_count(result.rows_per_sec as u64),
        result.mb_per_sec
    );
}

fn print_table_footer() {
    println!("└────────────┴────────────┴────────────┴────────────┴──────────────┴──────────────┘");
}

#[expect(
    dead_code,
    reason = "diagnostic helper used when benchmarks fail; not reached on the happy path"
)]
fn print_error_row(query: &str, rows: u64, error: &str) {
    println!(
        "│ {:>10} │ {:>10} │ {:^44} │",
        query,
        format_count(rows),
        format!("ERROR: {}", &error[..error.len().min(38)])
    );
}

fn run_insert_benchmark(connection: &Connection, row_count: i64) -> Result<BenchmarkResult> {
    println!("\n=== Insert Benchmark ===");

    // Drop and recreate table for clean benchmark
    connection.execute_command("DROP TABLE IF EXISTS measurements")?;
    let catalog = Catalog::new(connection);
    let table_def = TableDefinition::new("measurements")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("sensor_id", SqlType::int())
        .add_nullable_column("value", SqlType::double())
        .add_nullable_column("timestamp", SqlType::big_int());
    catalog.create_table(&table_def)?;

    println!("Inserting {row_count} rows using COPY protocol...");
    println!("(monitoring CPU/memory every {SAMPLE_INTERVAL_MS}ms)");

    // Start resource monitoring
    let monitor = ResourceMonitor::start();

    let start = std::time::Instant::now();
    {
        let mut inserter = Inserter::new(connection, &table_def)?;

        for i in 0..row_count {
            let id = i as i32;
            let sensor_id = (i % 10) as i32;
            let value = (i as f64) * 0.1;
            let timestamp = 1700000000000i64 + i * 1000;

            // Simple API - just add_row with values
            inserter.add_row(&[&id, &sensor_id, &value, &timestamp])?;
        }

        inserter.execute()?;
    }
    let elapsed = start.elapsed();

    // Stop monitoring and collect stats
    let resource_stats = monitor.stop();

    let elapsed_secs = elapsed.as_secs_f64();
    let rows_per_sec = row_count as f64 / elapsed_secs;
    let total_bytes = (row_count as usize) * bytes_per_row();
    let throughput_mb_per_sec = mb_per_sec(total_bytes as f64, elapsed_secs);

    println!(
        "Inserted {row_count} rows in {elapsed_secs:.3} seconds ({rows_per_sec:.0} rows/sec, {throughput_mb_per_sec:.2} MB/sec)"
    );
    println!("\nResource Usage During Insert:");
    println!(
        "  CPU:    avg={:.1}%, max={:.1}%",
        resource_stats.cpu_avg(),
        resource_stats.cpu_max()
    );
    println!(
        "  Memory: avg={:.1} MB, max={:.1} MB, min={:.1} MB",
        resource_stats.memory_avg_mb(),
        resource_stats.memory_max_mb(),
        resource_stats.memory_min_mb()
    );
    println!("  Samples: {}", resource_stats.sample_count);

    Ok(BenchmarkResult {
        elapsed_secs,
        rows_per_sec,
        resource_stats,
    })
}

/// Runs the multi-threaded insert benchmark using `InsertChunk` and `ChunkSender`.
fn run_threaded_insert_benchmark(
    connection: &Connection,
    row_count: i64,
    num_workers: usize,
    rows_per_chunk: usize,
) -> Result<BenchmarkResult> {
    println!("\n=== Threaded Insert Benchmark ===");
    println!("Workers: {num_workers}, Rows per chunk: {rows_per_chunk}");

    // Drop and recreate table for clean benchmark
    connection.execute_command("DROP TABLE IF EXISTS measurements_threaded")?;
    let catalog = Catalog::new(connection);
    let table_def = TableDefinition::new("measurements_threaded")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("sensor_id", SqlType::int())
        .add_nullable_column("value", SqlType::double())
        .add_nullable_column("timestamp", SqlType::big_int());
    catalog.create_table(&table_def)?;

    println!("Inserting {row_count} rows using multi-threaded ChunkSender...");
    println!("(monitoring CPU/memory every {SAMPLE_INTERVAL_MS}ms)");

    // Start resource monitoring
    let monitor = ResourceMonitor::start();

    let start = std::time::Instant::now();

    // Create the chunk sender (protected by mutex internally)
    let sender = ChunkSender::new(connection, &table_def)?;

    // Create channel for sending chunks from workers to sender thread
    let (tx, rx) = mpsc::channel::<InsertChunk>();

    // Calculate rows per worker
    let rows_per_worker = row_count / num_workers as i64;
    let remainder = row_count % num_workers as i64;

    // Spawn worker threads
    let table_def_arc = Arc::new(table_def);
    let handles: Vec<thread::JoinHandle<Result<()>>> = (0..num_workers)
        .map(|worker_id| {
            let tx = tx.clone();
            let table_def = Arc::clone(&table_def_arc);

            // Give extra rows to last worker
            let worker_rows = if worker_id == num_workers - 1 {
                rows_per_worker + remainder
            } else {
                rows_per_worker
            };

            // Calculate starting ID for this worker
            let start_id = worker_id as i64 * rows_per_worker;

            thread::spawn(move || {
                benchmark_worker_thread(
                    worker_id,
                    start_id,
                    worker_rows,
                    rows_per_chunk,
                    &table_def,
                    tx,
                )
            })
        })
        .collect();

    // Drop the original sender so rx.iter() will terminate when all workers finish
    drop(tx);

    // Sender thread: receive chunks and send to Hyper
    let mut chunks_received = 0usize;
    for chunk in &rx {
        sender.send_chunk(chunk)?;
        chunks_received += 1;
    }

    // Wait for all workers to complete
    for handle in handles {
        handle.join().expect("Worker thread panicked")?;
    }

    // Finish the COPY operation
    let total_rows = sender.finish()?;
    let elapsed = start.elapsed();

    // Stop monitoring and collect stats
    let resource_stats = monitor.stop();

    let elapsed_secs = elapsed.as_secs_f64();
    let rows_per_sec = total_rows as f64 / elapsed_secs;
    let total_bytes = (total_rows as usize) * bytes_per_row();
    let throughput_mb_per_sec = mb_per_sec(total_bytes as f64, elapsed_secs);

    println!(
        "Inserted {total_rows} rows in {elapsed_secs:.3} seconds ({rows_per_sec:.0} rows/sec, {throughput_mb_per_sec:.2} MB/sec)"
    );
    println!("Chunks sent: {chunks_received}");
    println!("\nResource Usage During Threaded Insert:");
    println!(
        "  CPU:    avg={:.1}%, max={:.1}%",
        resource_stats.cpu_avg(),
        resource_stats.cpu_max()
    );
    println!(
        "  Memory: avg={:.1} MB, max={:.1} MB, min={:.1} MB",
        resource_stats.memory_avg_mb(),
        resource_stats.memory_max_mb(),
        resource_stats.memory_min_mb()
    );
    println!("  Samples: {}", resource_stats.sample_count);

    Ok(BenchmarkResult {
        elapsed_secs,
        rows_per_sec,
        resource_stats,
    })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "call-site ergonomics: function consumes logically-owned parameters, refactoring signatures is not worth per-site churn"
)]
/// Worker thread for the threaded insert benchmark.
fn benchmark_worker_thread(
    _worker_id: usize,
    start_id: i64,
    total_rows: i64,
    rows_per_chunk: usize,
    table_def: &TableDefinition,
    tx: mpsc::Sender<InsertChunk>,
) -> Result<()> {
    // Create initial chunk
    let mut chunk = InsertChunk::from_table_definition(table_def);

    for i in 0..total_rows {
        let id = (start_id + i) as i32;
        let sensor_id = id % 10;
        let value = f64::from(id) * 0.1;
        let timestamp = 1700000000000i64 + i64::from(id) * 1000;

        // Encode the row
        chunk.add_i32(id)?;
        chunk.add_i32(sensor_id)?;
        chunk.add_f64(value)?;
        chunk.add_i64(timestamp)?;
        chunk.end_row()?;

        // Send chunk when it reaches the target size
        if chunk.row_count() >= rows_per_chunk || chunk.should_flush() {
            tx.send(chunk)
                .map_err(|e| hyperdb_api::Error::internal(format!("Channel send failed: {e}")))?;
            chunk = InsertChunk::from_table_definition(table_def);
        }
    }

    // Send any remaining rows
    if !chunk.is_empty() {
        tx.send(chunk)
            .map_err(|e| hyperdb_api::Error::internal(format!("Channel send failed: {e}")))?;
    }

    Ok(())
}

/// Validates that the threaded insert data is correct.
fn validate_threaded_insert(connection: &Connection, expected_row_count: i64) -> Result<()> {
    println!("\n=== Validating Threaded Insert ===");

    // Check row count
    let actual_count: i64 = connection
        .execute_scalar_query::<i64>("SELECT COUNT(*) FROM measurements_threaded")?
        .ok_or_else(|| hyperdb_api::Error::internal("Failed to get row count"))?;

    if actual_count != expected_row_count {
        return Err(hyperdb_api::Error::internal(format!(
            "Row count mismatch! Expected {expected_row_count}, got {actual_count}"
        )));
    }
    println!("[OK] Row count verified: {actual_count} rows");

    // Validate aggregations match expected values
    let mut result = connection.execute_query(
        "SELECT sensor_id, COUNT(*), SUM(id::BIGINT) FROM measurements_threaded GROUP BY sensor_id ORDER BY sensor_id",
    )?;
    println!("[OK] Aggregation check (per sensor_id):");
    let rows_per_sensor = expected_row_count / 10;
    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            let sensor_id: i32 = row.get(0).unwrap_or(-1);
            let count: i64 = row.get(1).unwrap_or(-1);
            let _sum_id: i64 = row.get(2).unwrap_or(-1);

            if count != rows_per_sensor {
                return Err(hyperdb_api::Error::internal(format!(
                    "Count mismatch for sensor_id={sensor_id}: expected {rows_per_sensor}, got {count}"
                )));
            }
        }
    }

    println!("[OK] Threaded insert validation passed!");
    Ok(())
}

/// Streaming aggregation stats - computed on the fly without keeping rows in memory.
#[derive(Debug, Default)]
struct StreamingStats {
    count: i64,
    sum_id: i64,
    sum_value: f64,
    min_id: i32,
    max_id: i32,
}

impl StreamingStats {
    fn new() -> Self {
        StreamingStats {
            count: 0,
            sum_id: 0,
            sum_value: 0.0,
            min_id: i32::MAX,
            max_id: i32::MIN,
        }
    }

    fn add(&mut self, id: i32, value: f64) {
        self.count += 1;
        self.sum_id += i64::from(id);
        self.sum_value += value;
        self.min_id = self.min_id.min(id);
        self.max_id = self.max_id.max(id);
    }
}

fn run_query_benchmarks(connection: &Connection, row_count: i64) -> Result<()> {
    print_header("QUERY BENCHMARKS (STREAMING)");

    // Drop and recreate table for query benchmarks
    connection.execute_command("DROP TABLE IF EXISTS query_test")?;
    let catalog = Catalog::new(connection);
    let table_def = TableDefinition::new("query_test")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("sensor_id", SqlType::int())
        .add_nullable_column("value", SqlType::double())
        .add_nullable_column("timestamp", SqlType::big_int());
    catalog.create_table(&table_def)?;

    // Use GENERATE_SERIES for fast data population
    println!("Populating table with {row_count} rows using GENERATE_SERIES...");
    let start = std::time::Instant::now();
    connection.execute_command(&format!(
        "INSERT INTO query_test
         SELECT s::INTEGER AS id, (s % 10)::INTEGER AS sensor_id, s * 0.1 AS value,
                1700000000000::BIGINT + s::BIGINT * 1000 AS timestamp
         FROM GENERATE_SERIES(0, {}) AS t(s)",
        row_count - 1
    ))?;
    let elapsed = start.elapsed();
    println!("Data generation: {:.3} seconds", elapsed.as_secs_f64());

    // Start resource monitoring for query benchmarks
    let monitor = ResourceMonitor::start();

    let mut query_results: Vec<QueryBenchmarkResult> = Vec::new();

    // Benchmark 1: Full table scan with streaming (default 64K chunk size)
    print_section("Query 1: Full Table Scan");
    println!("(streaming with 64K row chunks - constant memory)");
    let start = std::time::Instant::now();
    let mut result = connection.execute_query("SELECT * FROM query_test")?;
    let mut stats = StreamingStats::new();
    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            // Extract values using lightweight row accessors
            let id = row.get_i32(0).unwrap_or(0);
            let value = row.get_f64(2).unwrap_or(0.0);
            stats.add(id, value);
        }
    }
    drop(result); // Explicitly drop to release connection
    let elapsed = start.elapsed();
    let total_bytes = (stats.count as usize) * bytes_per_row();
    let query_result = QueryBenchmarkResult::new(
        "Full Scan".to_string(),
        stats.count as u64,
        total_bytes,
        elapsed,
    );
    query_results.push(query_result.clone());
    println!(
        "  Aggregates: sum_id={}, min_id={}, max_id={}",
        stats.sum_id, stats.min_id, stats.max_id
    );

    // Benchmark 2: Filtered query with streaming
    print_section("Query 2: Filtered Query");
    let start = std::time::Instant::now();
    let mut result = connection.execute_query("SELECT * FROM query_test WHERE sensor_id = 5")?;
    let mut stats = StreamingStats::new();
    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            let id = row.get_i32(0).unwrap_or(0);
            let value = row.get_f64(2).unwrap_or(0.0);
            stats.add(id, value);
        }
    }
    drop(result);
    let elapsed = start.elapsed();
    let total_bytes = (stats.count as usize) * bytes_per_row();
    let query_result = QueryBenchmarkResult::new(
        "Filtered".to_string(),
        stats.count as u64,
        total_bytes,
        elapsed,
    );
    query_results.push(query_result.clone());

    // Benchmark 3: Server-side aggregation query (returns only 10 rows)
    print_section("Query 3: Server-side Aggregation");
    let start = std::time::Instant::now();
    let mut result = connection.execute_query(
        "SELECT sensor_id, AVG(value), COUNT(*) FROM query_test GROUP BY sensor_id ORDER BY sensor_id",
    )?;
    let mut agg_row_count = 0;
    let mut total_count: i64 = 0;
    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            let count = row.get_i64(2).unwrap_or(0);
            total_count += count;
            agg_row_count += 1;
        }
    }
    drop(result);
    let elapsed = start.elapsed();
    // For aggregation query, use small result set size (10 rows, minimal data)
    let agg_bytes = agg_row_count * 32; // Approximate bytes for 3 columns × 10 rows
    let query_result = QueryBenchmarkResult::new(
        "Aggregation".to_string(),
        total_count as u64,
        agg_bytes as usize,
        elapsed,
    );
    query_results.push(query_result.clone());
    println!("  Retrieved {agg_row_count} groups (total rows: {total_count})");

    // Display results in table format
    print_section("Query Benchmark Results");
    print_table_header();
    for result in &query_results {
        print_table_row(result);
    }
    print_table_footer();

    // Stop monitoring and show resource usage
    let resource_stats = monitor.stop();
    println!("\nResource Usage During Query Benchmarks:");
    println!(
        "  CPU:    avg={:.1}%, max={:.1}%",
        resource_stats.cpu_avg(),
        resource_stats.cpu_max()
    );
    println!(
        "  Memory: avg={:.1} MB, max={:.1} MB, min={:.1} MB",
        resource_stats.memory_avg_mb(),
        resource_stats.memory_max_mb(),
        resource_stats.memory_min_mb()
    );

    Ok(())
}

/// Validates that inserted data persists after Hyper restart.
/// This ensures the benchmark isn't just measuring buffered writes.
fn validate_insert_persistence(connection: &Connection, expected_row_count: i64) -> Result<()> {
    println!("\n=== Validating Insert Persistence ===");

    // Check row count using scalar query
    let actual_count: i64 = connection
        .execute_scalar_query::<i64>("SELECT COUNT(*) FROM measurements")?
        .ok_or_else(|| hyperdb_api::Error::internal("Failed to get row count"))?;

    if actual_count != expected_row_count {
        return Err(hyperdb_api::Error::internal(format!(
            "Row count mismatch! Expected {expected_row_count}, got {actual_count}"
        )));
    }
    println!("[OK] Row count verified: {actual_count} rows");

    // Validate first few rows
    let mut result = connection.execute_query(
        "SELECT id, sensor_id, value, timestamp FROM measurements ORDER BY id LIMIT 5",
    )?;
    println!("[OK] First 5 rows:");
    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            let id: i32 = row.get(0).unwrap_or(-1);
            let sensor_id: i32 = row.get(1).unwrap_or(-1);
            let value: f64 = row.get(2).unwrap_or(-1.0);
            let timestamp: i64 = row.get(3).unwrap_or(-1);
            println!("    id={id}, sensor_id={sensor_id}, value={value:.1}, timestamp={timestamp}");

            // Validate the data matches expected pattern
            let expected_sensor_id = id % 10;
            let expected_value = f64::from(id) * 0.1;
            let expected_timestamp = 1700000000000i64 + i64::from(id) * 1000;

            if sensor_id != expected_sensor_id
                || (value - expected_value).abs() > 0.001
                || timestamp != expected_timestamp
            {
                return Err(hyperdb_api::Error::internal(format!(
                    "Data mismatch at id={id}: got sensor_id={sensor_id}, value={value}, timestamp={timestamp}, expected sensor_id={expected_sensor_id}, value={expected_value}, timestamp={expected_timestamp}"
                )));
            }
        }
    }

    // Validate last few rows
    let mut result = connection.execute_query(&format!(
        "SELECT id, sensor_id, value, timestamp FROM measurements WHERE id >= {} ORDER BY id LIMIT 5",
        expected_row_count - 5
    ))?;
    println!("[OK] Last 5 rows:");
    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            let id: i32 = row.get(0).unwrap_or(-1);
            let sensor_id: i32 = row.get(1).unwrap_or(-1);
            let value: f64 = row.get(2).unwrap_or(-1.0);
            let timestamp: i64 = row.get(3).unwrap_or(-1);
            println!("    id={id}, sensor_id={sensor_id}, value={value:.1}, timestamp={timestamp}");

            // Validate the data matches expected pattern
            let expected_sensor_id = id % 10;
            let expected_value = f64::from(id) * 0.1;
            let expected_timestamp = 1700000000000i64 + i64::from(id) * 1000;

            if sensor_id != expected_sensor_id
                || (value - expected_value).abs() > 0.001
                || timestamp != expected_timestamp
            {
                return Err(hyperdb_api::Error::internal(format!(
                    "Data mismatch at id={id}: got sensor_id={sensor_id}, value={value}, timestamp={timestamp}, expected sensor_id={expected_sensor_id}, value={expected_value}, timestamp={expected_timestamp}"
                )));
            }
        }
    }

    // Validate aggregations match expected values
    let mut result = connection.execute_query(
        "SELECT sensor_id, COUNT(*), SUM(id::BIGINT) FROM measurements GROUP BY sensor_id ORDER BY sensor_id",
    )?;
    println!("[OK] Aggregation check (per sensor_id):");
    let rows_per_sensor = expected_row_count / 10;
    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            let sensor_id: i32 = row.get(0).unwrap_or(-1);
            let count: i64 = row.get(1).unwrap_or(-1);
            let sum_id: i64 = row.get(2).unwrap_or(-1);

            if count != rows_per_sensor {
                return Err(hyperdb_api::Error::internal(format!(
                    "Count mismatch for sensor_id={sensor_id}: expected {rows_per_sensor}, got {count}"
                )));
            }
            println!("    sensor_id={sensor_id}: count={count}, sum_id={sum_id}");
        }
    }

    println!("[OK] All validations passed!");
    Ok(())
}

/// Runs a single insert benchmark with the given transport mode.
fn run_transport_benchmark(
    transport_mode: TransportMode,
    row_count: i64,
    db_path: &str,
) -> Result<BenchmarkResult> {
    use hyperdb_api::Parameters;
    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    params.set_transport_mode(transport_mode);

    let mode_name = match transport_mode {
        TransportMode::Ipc => {
            #[cfg(unix)]
            {
                "IPC (Unix Socket)"
            }
            #[cfg(windows)]
            {
                "IPC (Named Pipe)"
            }
            #[cfg(not(any(unix, windows)))]
            {
                "IPC"
            }
        }
        TransportMode::Tcp => "TCP",
    };

    println!("\n--- {mode_name} Mode ---");

    let hyper = HyperProcess::new(None, Some(&params))?;
    println!("  Transport: {:?}", hyper.transport_mode());

    let connection = Connection::new(&hyper, db_path, CreateMode::CreateAndReplace)?;

    run_insert_benchmark(&connection, row_count)
}

/// Result of a single query pass in the TCP-vs-gRPC comparison phase:
/// time elapsed, rows counted, bytes transferred, plus the peak memory
/// sampled during the run.
struct TcpVsGrpcResult {
    label: &'static str,
    elapsed_secs: f64,
    rows: i64,
    bytes: usize,
    resource_stats: ResourceStats,
}

fn bind_ephemeral_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| hyperdb_api::Error::internal(format!("failed to bind ephemeral port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| hyperdb_api::Error::internal(format!("local_addr: {e}")))?
        .port();
    // Listener drops here, releasing the port for hyperd to claim. There is
    // a small race window between this function returning and hyperd
    // binding, which is acceptable for a local benchmark.
    drop(listener);
    Ok(port)
}

/// SQL query that synthesizes rows matching the `measurements` schema
/// (id INT, `sensor_id` INT, value DOUBLE, timestamp BIGINT) directly from
/// `generate_series`. Used so both TCP and gRPC can run the exact same
/// query with no attached database and no authentication dependency —
/// hyperd's gRPC surface refuses to attach locally-created `.hyper` files
/// (the gRPC session has no role on them), so a populated-table comparison
/// is not possible with the bundled `hyperd`. Computing on the fly with
/// `generate_series` sidesteps that while keeping the same data shape,
/// size, and schema that `measurements` queries would produce.
const TCP_VS_GRPC_QUERY_TEMPLATE: &str = r"SELECT
    i::INT AS id,
    (i % 10)::INT AS sensor_id,
    (i::DOUBLE PRECISION * 0.1) AS value,
    (1700000000000::BIGINT + i::BIGINT * 1000) AS timestamp
FROM generate_series(1, {row_count}) AS s(i)";

fn tcp_vs_grpc_query(row_count: i64) -> String {
    TCP_VS_GRPC_QUERY_TEMPLATE.replace("{row_count}", &row_count.to_string())
}

/// Runs a TCP-vs-gRPC query comparison on a synthesized 100M-row 4-column
/// result that matches the `measurements` schema. One Hyper process in
/// `ListenMode::Both` serves both transports, and the query has no
/// attached-database dependency, so the comparison isolates
/// transport + decode cost.
///
/// Produces three rows:
/// - TCP (streaming)
/// - gRPC streaming (the new `Connection::execute_query` → chunk stream path)
/// - gRPC buffered (the existing `execute_query_to_arrow` path — materializes
///   the whole Arrow IPC payload in client memory before decoding)
fn run_tcp_vs_grpc_query_benchmark(row_count: i64, _db_path: &str) -> Result<()> {
    use hyperdb_api::Parameters;

    print_header("TCP vs gRPC Query Comparison (4-column synthesized schema)");

    let grpc_port = bind_ephemeral_port()?;
    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    params.set_listen_mode(hyperdb_api::ListenMode::Both { grpc_port });
    // Force TCP for the libpq side so host:port endpoints work cleanly.
    params.set_transport_mode(TransportMode::Tcp);
    let hyper = HyperProcess::new(None, Some(&params))?;

    let grpc_url = hyper
        .grpc_url()
        .ok_or_else(|| hyperdb_api::Error::internal("Both mode did not expose a gRPC URL"))?;
    println!("  TCP endpoint:  {}", hyper.require_endpoint()?);
    println!("  gRPC URL:      {grpc_url}");
    println!(
        "  Schema:        (id INT, sensor_id INT, value DOUBLE, timestamp BIGINT), 24 bytes/row"
    );
    println!(
        "  Rows:          {} ({} total, ~{:.2} GB of Arrow IPC)",
        row_count,
        format_row_count(row_count),
        (row_count as f64 * bytes_per_row() as f64) / 1_000_000_000.0
    );

    let query = tcp_vs_grpc_query(row_count);

    // TCP query — hyperd computes the result and streams it back. We
    // still need a database connection for the libpq session to work;
    // point it at a scratch temp file since the query itself references
    // no tables.
    let tmp = tempfile::tempdir()
        .map_err(|e| hyperdb_api::Error::internal(format!("failed to create tempdir: {e}")))?;
    let scratch_db = tmp.path().join("tcp_vs_grpc_scratch.hyper");

    let tcp_result = {
        let connection = Connection::new(&hyper, &scratch_db, CreateMode::CreateAndReplace)?;
        measure_query("TCP (streaming)", &connection, row_count, &query, None)?
    };

    // Use SYNC transfer mode for a fair TCP-vs-gRPC comparison: in SYNC
    // the server streams the whole result as one server-streaming RPC,
    // which mirrors TCP's COPY TO STDOUT shape and avoids the
    // per-ADAPTIVE-chunk round-trips whose row-count cap ("the server
    // stops after one chunk and the client has to ask for more") would
    // under-report row counts if the client doesn't poll repeatedly.
    let transfer_mode = hyperdb_api::grpc::TransferMode::Sync;

    // Test buffered first to verify the server can send all rows before
    // diagnosing streaming.
    let grpc_buffered_result = {
        let connection = hyperdb_api::ConnectionBuilder::new(&grpc_url)
            .create_mode(CreateMode::DoNotCreate)
            .transfer_mode(transfer_mode)
            .build()?;
        measure_buffered_grpc_query("gRPC buffered", &connection, row_count, &query)?
    };

    // gRPC query via Connection::execute_query — hits the new streaming
    // path. gRPC doesn't need a database for a query that only references
    // generate_series, so we build a Connection with no `.database()` and
    // the required `CreateMode::DoNotCreate`.
    let grpc_stream_result = {
        let connection = hyperdb_api::ConnectionBuilder::new(&grpc_url)
            .create_mode(CreateMode::DoNotCreate)
            .transfer_mode(transfer_mode)
            .build()?;
        measure_query(
            "gRPC streaming",
            &connection,
            row_count,
            &query,
            /* arrow_bytes_override */ None,
        )?
    };

    // Print side-by-side comparison.
    println!();
    println!(
        "┌─────────────────────────────┬──────────┬──────────────┬─────────────┬─────────────┐"
    );
    println!(
        "│ {:<27} │ {:>8} │ {:>12} │ {:>11} │ {:>11} │",
        "Mode", "Time (s)", "Rows/sec", "MB/sec", "Peak Mem"
    );
    println!(
        "├─────────────────────────────┼──────────┼──────────────┼─────────────┼─────────────┤"
    );
    for r in [&tcp_result, &grpc_stream_result, &grpc_buffered_result] {
        let rows_per_sec = r.rows as f64 / r.elapsed_secs;
        let mb = r.bytes as f64 / 1_000_000.0;
        let mbps = mb / r.elapsed_secs;
        println!(
            "│ {:<27} │ {:>8.2} │ {:>12} │ {:>8.1} MB │ {:>8.1} MB │",
            r.label,
            r.elapsed_secs,
            format_count(rows_per_sec as u64),
            mbps,
            r.resource_stats.memory_max_mb()
        );
    }
    println!(
        "└─────────────────────────────┴──────────┴──────────────┴─────────────┴─────────────┘"
    );

    println!();
    println!("Notes: Both transports run the same `generate_series` query (no stored table, no");
    println!("       attached database). TCP uses libpq COPY TO STDOUT (arrowstream) and");
    println!("       returns every row. gRPC streaming uses `GrpcChunkStream` +");
    println!("       `ArrowRowset::from_stream`, decoding record batches one chunk at a time.");
    println!("       gRPC buffered uses `execute_query_to_arrow`, which collects the full");
    println!("       payload in memory (and pays an additional concat memcpy if the server");
    println!("       streamed multiple chunks). Above ~700K rows hyperd's bundled gRPC");
    println!("       service truncates results after its first inline chunk batch; rows/sec");
    println!("       and MB/sec are computed on the data actually delivered.");

    // Explicitly drop things in dependency order.
    drop(hyper);
    Ok(())
}

/// Streams the configured query through `connection`, counting rows and
/// tracking peak memory. Used for both TCP and the streaming gRPC path
/// because `Connection::execute_query` returns a `Rowset` whose
/// `next_chunk()` semantics are identical on both transports.
fn measure_query(
    label: &'static str,
    connection: &Connection,
    row_count: i64,
    query: &str,
    arrow_bytes_override: Option<usize>,
) -> Result<TcpVsGrpcResult> {
    let monitor = ResourceMonitor::start();
    let start = std::time::Instant::now();
    let mut result = connection.execute_query(query)?;
    let mut rows = 0i64;
    while let Some(chunk) = result.next_chunk()? {
        rows += chunk.len() as i64;
    }
    let elapsed_secs = start.elapsed().as_secs_f64();
    let resource_stats = monitor.stop();

    let bytes = arrow_bytes_override.unwrap_or_else(|| (rows as usize) * bytes_per_row());
    if rows != row_count {
        println!(
            "  {label}: WARNING got {rows}/{row_count} rows (server-side truncation; reporting throughput on received data only)"
        );
    }

    println!(
        "  {label}: {:.3}s, {} rows ({} rows/sec)",
        elapsed_secs,
        format_count(rows as u64),
        format_count((rows as f64 / elapsed_secs) as u64),
    );

    Ok(TcpVsGrpcResult {
        label,
        elapsed_secs,
        rows,
        bytes,
        resource_stats,
    })
}

/// Runs the gRPC query via the buffered Arrow API (`execute_query_to_arrow`
/// + `ArrowRowset::from_bytes`) and counts rows. This materializes the full
///   Arrow IPC payload in client memory — the resource monitor captures how
///   much that costs.
fn measure_buffered_grpc_query(
    label: &'static str,
    connection: &Connection,
    row_count: i64,
    query: &str,
) -> Result<TcpVsGrpcResult> {
    let monitor = ResourceMonitor::start();
    let start = std::time::Instant::now();
    let arrow_data = connection.execute_query_to_arrow(query)?;
    let bytes = arrow_data.len();
    let mut rowset = hyperdb_api::ArrowRowset::from_bytes(arrow_data)?;
    let mut rows = 0i64;
    while let Some(chunk) = rowset.next_chunk()? {
        rows += chunk.len() as i64;
    }
    let elapsed_secs = start.elapsed().as_secs_f64();
    let resource_stats = monitor.stop();

    if rows != row_count {
        println!(
            "  {label}: WARNING got {rows}/{row_count} rows (server-side truncation; reporting throughput on received data only)"
        );
    }

    println!(
        "  {label}: {:.3}s, {} rows ({} rows/sec)",
        elapsed_secs,
        format_count(rows as u64),
        format_count((rows as f64 / elapsed_secs) as u64),
    );

    Ok(TcpVsGrpcResult {
        label,
        elapsed_secs,
        rows,
        bytes,
        resource_stats,
    })
}

fn main() -> Result<()> {
    let row_count = get_row_count();
    let db_path = "test_results/benchmark.hyper";
    let log_path = "test_results/benchmark.log";

    // Configuration for threaded benchmark
    let num_workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .max(4); // Use available CPU cores, minimum 4
    let rows_per_chunk = 100_000; // 100K rows per chunk is a good balance

    // Create test_results directory if it doesn't exist
    std::fs::create_dir_all("test_results")?;

    // Configure HyperProcess to write logs to test_results
    use hyperdb_api::Parameters;
    let mut params = Parameters::new();
    params.set("log_dir", "test_results");

    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║                    Hyper API Performance Benchmark                           ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();
    println!("Configuration:");
    println!(
        "  Row count:       {} ({})",
        row_count,
        format_row_count(row_count)
    );
    println!("  Worker threads:  {num_workers}");
    println!("  Rows per chunk:  {rows_per_chunk}");

    // Phase 1: Single-threaded Insert benchmark
    let insert_result = {
        let hyper = HyperProcess::new(None, Some(&params))?;
        let connection = Connection::new(&hyper, db_path, CreateMode::CreateAndReplace)?;
        println!("\nCreated database");

        // Run single-threaded insert benchmark
        let result = run_insert_benchmark(&connection, row_count)?;

        println!("\nShutting down Hyper to verify data persistence...");
        result
    };

    // Phase 2: Restart and validate single-threaded insert
    {
        println!("Restarting Hyper...");
        let hyper = HyperProcess::new(None, Some(&params))?;
        let connection = Connection::new(&hyper, db_path, CreateMode::DoNotCreate)?;
        println!("Reconnected to database");

        // Validate inserted data persisted correctly
        validate_insert_persistence(&connection, row_count)?;
    }

    // Phase 3: Multi-threaded Insert benchmark
    let threaded_result = {
        let hyper = HyperProcess::new(None, Some(&params))?;
        let connection = Connection::new(&hyper, db_path, CreateMode::DoNotCreate)?;

        // Run multi-threaded insert benchmark
        run_threaded_insert_benchmark(&connection, row_count, num_workers, rows_per_chunk)?
    };

    // Phase 4: Validate threaded insert
    {
        let hyper = HyperProcess::new(None, Some(&params))?;
        let connection = Connection::new(&hyper, db_path, CreateMode::DoNotCreate)?;

        validate_threaded_insert(&connection, row_count)?;
    }

    // Phase 5: Query benchmarks
    {
        println!("\nRestarting Hyper for query benchmarks...");
        let hyper = HyperProcess::new(None, Some(&params))?;
        let connection = Connection::new(&hyper, db_path, CreateMode::DoNotCreate)?;

        // Run query benchmarks
        run_query_benchmarks(&connection, row_count)?;
    }

    // Phase 5b: TCP vs gRPC query comparison on the same populated
    // `measurements` table (reuses the single-threaded insert's output, so
    // no extra INSERT cost beyond the query work itself).
    run_tcp_vs_grpc_query_benchmark(row_count, db_path)?;

    // Print comparison summary
    let total_bytes = (row_count as usize) * bytes_per_row();
    let single_mb_per_sec = mb_per_sec(total_bytes as f64, insert_result.elapsed_secs);
    let threaded_mb_per_sec = mb_per_sec(total_bytes as f64, threaded_result.elapsed_secs);

    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║                         BENCHMARK COMPARISON                                 ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();
    println!("┌──────────────────────────┬────────────────────┬────────────────────┐");
    println!(
        "│ {:>24} │ {:>18} │ {:>18} │",
        "Metric", "Single-Threaded", "Multi-Threaded"
    );
    println!("├──────────────────────────┼────────────────────┼────────────────────┤");
    println!(
        "│ {:>24} │ {:>18} │ {:>18} │",
        "Rows",
        format_row_count(row_count),
        format_row_count(row_count)
    );
    println!(
        "│ {:>24} │ {:>15.3} s │ {:>15.3} s │",
        "Time", insert_result.elapsed_secs, threaded_result.elapsed_secs
    );
    println!(
        "│ {:>24} │ {:>14.0} r/s │ {:>14.0} r/s │",
        "Throughput (rows/sec)", insert_result.rows_per_sec, threaded_result.rows_per_sec
    );
    println!(
        "│ {:>24} │ {:>14.2} MB/s │ {:>14.2} MB/s │",
        "Throughput (MB/sec)", single_mb_per_sec, threaded_mb_per_sec
    );
    println!(
        "│ {:>24} │ {:>17.1}% │ {:>17.1}% │",
        "CPU Avg",
        insert_result.resource_stats.cpu_avg(),
        threaded_result.resource_stats.cpu_avg()
    );
    println!(
        "│ {:>24} │ {:>17.1}% │ {:>17.1}% │",
        "CPU Max",
        insert_result.resource_stats.cpu_max(),
        threaded_result.resource_stats.cpu_max()
    );
    println!(
        "│ {:>24} │ {:>15.1} MB │ {:>15.1} MB │",
        "Memory Avg",
        insert_result.resource_stats.memory_avg_mb(),
        threaded_result.resource_stats.memory_avg_mb()
    );
    println!(
        "│ {:>24} │ {:>15.1} MB │ {:>15.1} MB │",
        "Memory Max",
        insert_result.resource_stats.memory_max_mb(),
        threaded_result.resource_stats.memory_max_mb()
    );
    println!("└──────────────────────────┴────────────────────┴────────────────────┘");

    // Calculate speedup
    let speedup = insert_result.elapsed_secs / threaded_result.elapsed_secs;
    println!();
    if speedup > 1.0 {
        println!("Multi-threaded is {speedup:.2}x FASTER than single-threaded");
    } else if speedup < 1.0 {
        println!(
            "Multi-threaded is {:.2}x SLOWER than single-threaded (regression!)",
            1.0 / speedup
        );
    } else {
        println!("Performance is approximately equal");
    }

    // Print database file size before deletion
    if let Ok(metadata) = std::fs::metadata(db_path) {
        let size_bytes = metadata.len();
        let size_mb = size_bytes as f64 / (1024.0 * 1024.0);
        println!("\nDatabase file size: {size_mb:.2} MB ({size_bytes} bytes)");
    }

    // Clean up benchmark files
    let _ = std::fs::remove_file(db_path);

    // Phase 6: IPC vs TCP Transport Comparison
    {
        #[cfg(unix)]
        let ipc_label = "IPC (Unix Socket)";
        #[cfg(windows)]
        let ipc_label = "IPC (Named Pipe)";
        #[cfg(not(any(unix, windows)))]
        let ipc_label = "IPC";

        print_header("IPC vs TCP Transport Comparison");
        println!("Comparing {ipc_label} vs TCP performance...\n");

        // Run TCP benchmark
        let tcp_result = run_transport_benchmark(TransportMode::Tcp, row_count, db_path)?;
        let _ = std::fs::remove_file(db_path);

        // Run IPC benchmark
        let ipc_result = run_transport_benchmark(TransportMode::Ipc, row_count, db_path)?;
        let _ = std::fs::remove_file(db_path);

        // Print comparison
        let total_bytes = (row_count as usize) * bytes_per_row();
        let tcp_mb_per_sec = mb_per_sec(total_bytes as f64, tcp_result.elapsed_secs);
        let ipc_mb_per_sec = mb_per_sec(total_bytes as f64, ipc_result.elapsed_secs);

        println!();
        println!(
            "╔══════════════════════════════════════════════════════════════════════════════╗"
        );
        println!(
            "║                      IPC vs TCP TRANSPORT COMPARISON                         ║"
        );
        println!(
            "╚══════════════════════════════════════════════════════════════════════════════╝"
        );
        println!();
        println!("┌──────────────────────────┬────────────────────┬────────────────────┐");
        println!("│ {:>24} │ {:>18} │ {:>18} │", "Metric", "TCP", ipc_label);
        println!("├──────────────────────────┼────────────────────┼────────────────────┤");
        println!(
            "│ {:>24} │ {:>18} │ {:>18} │",
            "Rows",
            format_row_count(row_count),
            format_row_count(row_count)
        );
        println!(
            "│ {:>24} │ {:>15.3} s │ {:>15.3} s │",
            "Time", tcp_result.elapsed_secs, ipc_result.elapsed_secs
        );
        println!(
            "│ {:>24} │ {:>14.0} r/s │ {:>14.0} r/s │",
            "Throughput (rows/sec)", tcp_result.rows_per_sec, ipc_result.rows_per_sec
        );
        println!(
            "│ {:>24} │ {:>14.2} MB/s │ {:>14.2} MB/s │",
            "Throughput (MB/sec)", tcp_mb_per_sec, ipc_mb_per_sec
        );
        println!(
            "│ {:>24} │ {:>17.1}% │ {:>17.1}% │",
            "CPU Avg",
            tcp_result.resource_stats.cpu_avg(),
            ipc_result.resource_stats.cpu_avg()
        );
        println!(
            "│ {:>24} │ {:>17.1}% │ {:>17.1}% │",
            "CPU Max",
            tcp_result.resource_stats.cpu_max(),
            ipc_result.resource_stats.cpu_max()
        );
        println!(
            "│ {:>24} │ {:>15.1} MB │ {:>15.1} MB │",
            "Memory Avg",
            tcp_result.resource_stats.memory_avg_mb(),
            ipc_result.resource_stats.memory_avg_mb()
        );
        println!(
            "│ {:>24} │ {:>15.1} MB │ {:>15.1} MB │",
            "Memory Max",
            tcp_result.resource_stats.memory_max_mb(),
            ipc_result.resource_stats.memory_max_mb()
        );
        println!("└──────────────────────────┴────────────────────┴────────────────────┘");

        // Calculate speedup
        let ipc_speedup = tcp_result.elapsed_secs / ipc_result.elapsed_secs;
        println!();
        if ipc_speedup > 1.01 {
            println!(
                "IPC is {:.2}x FASTER than TCP ({:.1}% improvement)",
                ipc_speedup,
                (ipc_speedup - 1.0) * 100.0
            );
        } else if ipc_speedup < 0.99 {
            println!(
                "IPC is {:.2}x SLOWER than TCP ({:.1}% regression)",
                1.0 / ipc_speedup,
                (1.0 / ipc_speedup - 1.0) * 100.0
            );
        } else {
            println!("IPC and TCP performance are approximately equal");
        }
    }

    let _ = std::fs::remove_file(log_path);

    Ok(())
}
