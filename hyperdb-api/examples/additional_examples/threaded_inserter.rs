// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Example: Multi-threaded bulk insertion with `InsertChunk` and `ChunkSender`
//!
//! This example demonstrates how to:
//! - Use `InsertChunk` to encode data in parallel across multiple worker threads
//! - Use `ChunkSender` to safely send chunks to Hyper (mutex-protected)
//! - Implement a producer-consumer pattern with MPSC channels
//! - Generate synthetic data for benchmarking
//!
//! The pattern separates data encoding (CPU-bound) from network I/O, allowing
//! multiple cores to prepare data while a single thread handles transmission.
//!
//! Run with: cargo run -p hyperdb-api --example `threaded_inserter`

#![allow(
    clippy::cast_precision_loss,
    reason = "example throughput display; values bounded by single-run workload"
)]
// Example harness: row-count display and synthetic-data ID narrowing.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "example harness: demo counts narrow by construction"
)]

use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use hyperdb_api::{
    Catalog, ChunkSender, Connection, CreateMode, HyperProcess, InsertChunk, Parameters, Result,
    SqlType, TableDefinition,
};

/// Configuration for the threaded insert benchmark
struct Config {
    /// Number of worker threads encoding data
    num_workers: usize,
    /// Total rows to insert
    total_rows: u64,
    /// Rows per chunk before sending
    rows_per_chunk: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            num_workers: 4,
            total_rows: 1_000_000,
            rows_per_chunk: 50_000,
        }
    }
}

fn main() -> Result<()> {
    // Parse simple command-line args
    let config = parse_args();

    println!("=== Multi-threaded Inserter Example ===\n");
    println!("Configuration:");
    println!("  Workers:        {}", config.num_workers);
    println!("  Total rows:     {}", config.total_rows);
    println!("  Rows per chunk: {}", config.rows_per_chunk);
    println!();

    // Create test_results directory
    std::fs::create_dir_all("test_results")?;

    // Start Hyper
    println!("Starting Hyper process...");
    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    let hyper = HyperProcess::new(None, Some(&params))?;

    let db_path = "test_results/threaded_inserter.hyper";
    let connection = Connection::new(&hyper, db_path, CreateMode::CreateAndReplace)?;
    println!("Created database: {db_path}\n");

    // Create the table
    let table_def = create_table(&connection)?;

    // Run the multi-threaded insert
    let rows = run_threaded_insert(&connection, &table_def, &config)?;

    // Verify the results
    verify_results(&connection, rows)?;

    // Show file size
    if let Ok(metadata) = std::fs::metadata(db_path) {
        let size_mb = metadata.len() as f64 / (1024.0 * 1024.0);
        println!("\nDatabase file size: {size_mb:.2} MB");
    }

    println!("\nExample completed successfully!");
    Ok(())
}

fn parse_args() -> Config {
    let args: Vec<String> = std::env::args().collect();
    let mut config = Config::default();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--workers" | "-w" if i + 1 < args.len() => {
                config.num_workers = args[i + 1].parse().unwrap_or(config.num_workers);
                i += 1;
            }
            "--rows" | "-r" if i + 1 < args.len() => {
                config.total_rows = args[i + 1].parse().unwrap_or(config.total_rows);
                i += 1;
            }
            "--chunk-size" | "-c" if i + 1 < args.len() => {
                config.rows_per_chunk = args[i + 1].parse().unwrap_or(config.rows_per_chunk);
                i += 1;
            }
            "--help" | "-h" => {
                println!("Usage: threaded_inserter [OPTIONS]");
                println!();
                println!("Options:");
                println!("  -w, --workers <N>     Number of worker threads (default: 4)");
                println!("  -r, --rows <N>        Total rows to insert (default: 1000000)");
                println!("  -c, --chunk-size <N>  Rows per chunk (default: 50000)");
                println!("  -h, --help            Show this help");
                std::process::exit(0);
            }
            _ => {}
        }
        i += 1;
    }

    config
}

fn create_table(connection: &Connection) -> Result<TableDefinition> {
    let table_def = TableDefinition::new("sensor_data")
        .add_required_column("id", SqlType::big_int())
        .add_required_column("sensor_id", SqlType::int())
        .add_nullable_column("temperature", SqlType::double())
        .add_nullable_column("humidity", SqlType::double())
        .add_nullable_column("timestamp", SqlType::big_int())
        .add_nullable_column("location", SqlType::text());

    Catalog::new(connection).create_table(&table_def)?;
    println!("Created table 'sensor_data' with 6 columns");

    Ok(table_def)
}

fn run_threaded_insert(
    connection: &Connection,
    table_def: &TableDefinition,
    config: &Config,
) -> Result<u64> {
    println!("\n--- Starting Multi-threaded Insert ---\n");

    // Create the chunk sender (protected by mutex internally)
    let sender = ChunkSender::new(connection, table_def)?;

    // Create channel for sending chunks from workers to sender thread
    let (tx, rx) = mpsc::channel::<InsertChunk>();

    // Calculate rows per worker
    let rows_per_worker = config.total_rows / config.num_workers as u64;
    let remainder = config.total_rows % config.num_workers as u64;

    let start = Instant::now();

    // Spawn worker threads
    let table_def = Arc::new(table_def.clone());
    let handles: Vec<JoinHandle<Result<WorkerStats>>> = (0..config.num_workers)
        .map(|worker_id| {
            let tx = tx.clone();
            let table_def = Arc::clone(&table_def);
            let rows_per_chunk = config.rows_per_chunk;

            // Give extra rows to last worker
            let worker_rows = if worker_id == config.num_workers - 1 {
                rows_per_worker + remainder
            } else {
                rows_per_worker
            };

            // Calculate starting ID for this worker
            let start_id = worker_id as u64 * rows_per_worker;

            thread::spawn(move || {
                worker_thread(
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
    let mut send_errors = Vec::new();

    for chunk in &rx {
        chunks_received += 1;
        if let Err(e) = sender.send_chunk(chunk) {
            send_errors.push(format!("Chunk {chunks_received}: {e}"));
        }

        // Progress update every 10 chunks
        if chunks_received % 10 == 0 {
            let elapsed = start.elapsed();
            let rows_so_far = sender.total_rows();
            let rate = rows_so_far as f64 / elapsed.as_secs_f64();
            println!(
                "  Progress: {chunks_received} chunks, {rows_so_far} rows, {rate:.0} rows/sec"
            );
        }
    }

    // Wait for all workers to complete
    let mut worker_stats = Vec::new();
    for handle in handles {
        match handle.join() {
            Ok(Ok(stats)) => worker_stats.push(stats),
            Ok(Err(e)) => send_errors.push(format!("Worker error: {e}")),
            Err(e) => send_errors.push(format!("Worker panic: {e:?}")),
        }
    }

    // Report any errors
    if !send_errors.is_empty() {
        println!("\nErrors during insert:");
        for err in &send_errors {
            println!("  - {err}");
        }
    }

    // Finish the COPY operation
    let total_rows = sender.finish()?;
    let elapsed = start.elapsed();

    // Print summary
    println!("\n--- Insert Complete ---\n");
    println!("Total rows inserted: {total_rows}");
    println!("Total chunks sent:   {chunks_received}");
    println!("Total time:          {elapsed:?}");
    println!(
        "Throughput:          {:.0} rows/sec",
        total_rows as f64 / elapsed.as_secs_f64()
    );

    // Print per-worker stats
    println!("\nWorker Statistics:");
    for stats in &worker_stats {
        println!(
            "  Worker {}: {} rows, {} chunks, {:?}",
            stats.worker_id, stats.rows_encoded, stats.chunks_created, stats.duration
        );
    }

    Ok(total_rows)
}

struct WorkerStats {
    worker_id: usize,
    rows_encoded: u64,
    chunks_created: usize,
    duration: std::time::Duration,
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "call-site ergonomics: function consumes logically-owned parameters, refactoring signatures is not worth per-site churn"
)]
fn worker_thread(
    worker_id: usize,
    start_id: u64,
    total_rows: u64,
    rows_per_chunk: usize,
    table_def: &TableDefinition,
    tx: Sender<InsertChunk>,
) -> Result<WorkerStats> {
    let start = Instant::now();
    let mut rows_encoded = 0u64;
    let mut chunks_created = 0usize;

    // Create initial chunk
    let mut chunk = InsertChunk::from_table_definition(table_def);

    // Sensor IDs cycle through 0-99
    let num_sensors = 100;

    for i in 0..total_rows {
        let id = (start_id + i) as i64;
        let sensor_id = ((start_id + i) % num_sensors) as i32;

        // Generate synthetic sensor data
        let temperature = 20.0 + (id % 30) as f64 + (id % 7) as f64 * 0.1;
        let humidity = 40.0 + (id % 50) as f64 + (id % 11) as f64 * 0.1;
        let timestamp = 1700000000000i64 + id * 1000; // 1 second intervals
        let location = format!("zone_{}", sensor_id % 10);

        // Encode the row
        chunk.add_i64(id)?;
        chunk.add_i32(sensor_id)?;
        chunk.add_f64(temperature)?;
        chunk.add_f64(humidity)?;
        chunk.add_i64(timestamp)?;
        chunk.add_str(&location)?;
        chunk.end_row()?;

        rows_encoded += 1;

        // Send chunk when it reaches the target size
        if chunk.row_count() >= rows_per_chunk || chunk.should_flush() {
            tx.send(chunk)
                .map_err(|e| hyperdb_api::Error::internal(format!("Channel send failed: {e}")))?;
            chunks_created += 1;
            chunk = InsertChunk::from_table_definition(table_def);
        }
    }

    // Send any remaining rows
    if !chunk.is_empty() {
        tx.send(chunk)
            .map_err(|e| hyperdb_api::Error::internal(format!("Channel send failed: {e}")))?;
        chunks_created += 1;
    }

    Ok(WorkerStats {
        worker_id,
        rows_encoded,
        chunks_created,
        duration: start.elapsed(),
    })
}

fn verify_results(connection: &Connection, expected_rows: u64) -> Result<()> {
    println!("\n--- Verifying Results ---\n");

    // Count rows
    let count: Option<i64> = connection.execute_scalar_query("SELECT COUNT(*) FROM sensor_data")?;
    let actual = count.unwrap_or(0) as u64;

    if actual == expected_rows {
        println!("[OK] Row count: {actual} (matches expected)");
    } else {
        println!("[ERROR] Row count: {actual} (expected {expected_rows})");
    }

    // Check ID range
    let min_id: Option<i64> = connection.execute_scalar_query("SELECT MIN(id) FROM sensor_data")?;
    let max_id: Option<i64> = connection.execute_scalar_query("SELECT MAX(id) FROM sensor_data")?;

    println!(
        "[OK] ID range: {} to {}",
        min_id.unwrap_or(0),
        max_id.unwrap_or(0)
    );

    // Sample some data
    println!("\nSample data (first 5 rows by ID):");
    let mut result = connection.execute_query(
        "SELECT id, sensor_id, temperature, humidity, timestamp, location 
         FROM sensor_data ORDER BY id LIMIT 5",
    )?;

    while let Some(chunk) = result.next_chunk()? {
        for row in &chunk {
            let id = row.get_i64(0).unwrap_or(0);
            let sensor_id = row.get_i32(1).unwrap_or(0);
            let temp = row.get_f64(2).unwrap_or(0.0);
            let humidity = row.get_f64(3).unwrap_or(0.0);
            let ts = row.get_i64(4).unwrap_or(0);
            let location = row.get::<String>(5).unwrap_or_default();

            println!(
                "  id={id}, sensor={sensor_id}, temp={temp:.1}C, humidity={humidity:.1}%, ts={ts}, loc={location}"
            );
        }
    }

    // Aggregate stats
    println!("\nAggregate statistics:");
    let avg_temp: Option<f64> =
        connection.execute_scalar_query("SELECT AVG(temperature) FROM sensor_data")?;
    let avg_humidity: Option<f64> =
        connection.execute_scalar_query("SELECT AVG(humidity) FROM sensor_data")?;
    let sensor_count: Option<i64> =
        connection.execute_scalar_query("SELECT COUNT(DISTINCT sensor_id) FROM sensor_data")?;

    println!("  Average temperature: {:.2}C", avg_temp.unwrap_or(0.0));
    println!("  Average humidity:    {:.2}%", avg_humidity.unwrap_or(0.0));
    println!("  Distinct sensors:    {}", sensor_count.unwrap_or(0));

    Ok(())
}
