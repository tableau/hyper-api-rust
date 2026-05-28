// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Comprehensive gRPC connectivity example for Hyper.
//!
//! This example demonstrates:
//! - Starting Hyper with gRPC endpoint (gRPC-only and combined modes)
//! - Query execution with Arrow IPC results
//! - Query builder integration with gRPC connections
//! - Transfer modes (SYNC, ADAPTIVE, ASYNC)
//! - Async gRPC usage
//! - Arrow data processing and aggregation
//! - Custom configuration options
//!
//! # Running this example
//!
//! ```bash
//! cargo run -p hyperdb-api --example grpc_query
//! ```
//!
//! The example will automatically start a Hyper server with gRPC enabled.
//!
//! # Manual Server Setup (Alternative)
//!
//! If you want to connect to an existing Hyper server:
//!
//! ```bash
//! # gRPC only
//! hyperd run --listen-connection "tcp.grpc://127.0.0.1:7484"
//!
//! # Both gRPC and libpq
//! hyperd run --listen-connection "tab.tcp://127.0.0.1:7483,tcp.grpc://127.0.0.1:7484"
//! ```

// Example harness: throughput math and row-count formatting narrow by construction.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "example harness: demo throughput/row-count display"
)]

mod grpc_example {
    use hyperdb_api::grpc::{GrpcConfig, GrpcConnection, GrpcConnectionAsync, TransferMode};
    use hyperdb_api::{HyperProcess, ListenMode, Parameters, Result};
    use std::time::{Duration, Instant};

    /// Demonstrates starting a `HyperProcess` with gRPC and running queries
    pub(crate) fn grpc_with_hyper_process() -> Result<()> {
        println!("=== Starting Hyper with gRPC Mode ===\n");

        // Start Hyper with gRPC only mode
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        params.set_listen_mode(ListenMode::Grpc { port: 0 }); // Auto-assign port
                                                              // Note: grpc_threads is automatically set by HyperProcess when gRPC mode is enabled

        println!("Starting HyperProcess with gRPC mode...");
        let hyper = HyperProcess::new(None, Some(&params))?;

        let grpc_endpoint = hyper
            .grpc_endpoint()
            .expect("gRPC endpoint should be available");
        let grpc_url = hyper.grpc_url().expect("gRPC URL should be available");
        println!("Hyper started!");
        println!("  gRPC endpoint: {grpc_endpoint}");
        println!("  gRPC URL: {grpc_url}");
        println!();

        // Connect via gRPC
        println!("Connecting to Hyper via gRPC...");
        // Note: For gRPC-only mode, we don't need a database file path for simple queries
        let config = GrpcConfig::new(&grpc_url);
        let mut conn = GrpcConnection::connect_with_config(config)?;
        println!("Connected successfully!\n");

        // Execute a simple query
        println!("Executing simple query: SELECT 1 as value, 'hello' as message");
        let result = conn.execute_query("SELECT 1 as value, 'hello' as message")?;
        println!("  Query ID: {:?}", result.query_id());
        println!("  Columns: {}", result.column_count());
        println!("  Arrow data size: {} bytes\n", result.arrow_data().len());

        // Execute a query with generate_series
        println!("Executing query with generate_series (1000 rows)...");
        let query = r"
            SELECT
                i as id,
                i * 2 as doubled,
                'row_' || CAST(i AS TEXT) as label
            FROM generate_series(1, 1000) as s(i)
        ";
        let start = Instant::now();
        let result = conn.execute_query(query)?;
        let elapsed = start.elapsed();
        println!("  Completed in {elapsed:?}");
        println!("  Arrow data size: {} bytes\n", result.arrow_data().len());

        // The HyperProcess will shut down automatically when it goes out of scope
        println!("Closing connection and shutting down Hyper...\n");
        drop(conn);
        drop(hyper);

        Ok(())
    }

    /// Demonstrates starting Hyper with both libpq and gRPC
    pub(crate) fn both_modes_example() -> Result<()> {
        println!("=== Starting Hyper with Both LibPq and gRPC ===\n");

        // Start Hyper with both modes
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        params.set_listen_mode(ListenMode::Both { grpc_port: 17484 }); // Specific port for gRPC
                                                                       // Note: grpc_threads is automatically set by HyperProcess when gRPC mode is enabled

        println!("Starting HyperProcess with both libpq and gRPC...");
        let hyper = HyperProcess::new(None, Some(&params))?;

        println!("Hyper started!");
        println!("  libpq endpoint: {}", hyper.endpoint().unwrap());
        println!("  gRPC endpoint: {}", hyper.grpc_endpoint().unwrap());
        println!("  gRPC URL: {}", hyper.grpc_url().unwrap());
        println!();

        // Connect via gRPC
        let grpc_url = hyper.grpc_url().unwrap();
        let config = GrpcConfig::new(&grpc_url);
        let mut grpc_conn = GrpcConnection::connect_with_config(config)?;

        // Run a query via gRPC
        println!("Running query via gRPC...");
        let result = grpc_conn.execute_query("SELECT 42 as answer")?;
        println!("  gRPC query result: {} bytes\n", result.arrow_data().len());

        // Note: You could also connect via libpq for write operations
        // let tcp_conn = Connection::connect(hyper.endpoint(), CreateMode::None)?;

        println!("Shutting down...\n");
        Ok(())
    }

    /// Demonstrates a large query using `generate_series`
    pub(crate) fn large_query_example() -> Result<()> {
        println!("=== Large Query Example (generate_series) ===\n");

        // Start Hyper with gRPC
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        params.set_listen_mode(ListenMode::Grpc { port: 0 });
        let hyper = HyperProcess::new(None, Some(&params))?;
        let grpc_url = hyper.grpc_url().unwrap();

        let config = GrpcConfig::new(&grpc_url);
        let mut conn = GrpcConnection::connect_with_config(config)?;

        // Generate 1,000 rows (reduced for faster example execution)
        let row_count = 1_000;
        let query = format!(
            r"
            SELECT
                i as id,
                i * 2 as doubled,
                i % 10000 as bucket,
                'row_' || CAST(i AS TEXT) as label,
                CASE WHEN i % 2 = 0 THEN true ELSE false END as is_even,
                i * 0.001 as fraction
            FROM generate_series(1, {row_count}) as s(i)
            "
        );

        println!("Executing query to generate {row_count} rows...");
        let start = Instant::now();
        let result = conn.execute_query(&query)?;
        let elapsed = start.elapsed();

        let arrow_data = result.arrow_data();
        println!("Query completed in {elapsed:?}");
        println!("Result statistics:");
        println!("  Rows requested: {row_count}");
        println!("  Arrow data size: {} bytes", arrow_data.len());
        println!(
            "  Bytes per row: {:.2}",
            arrow_data.len() as f64 / f64::from(row_count)
        );
        println!(
            "  Throughput: {:.2} MB/s",
            (arrow_data.len() as f64 / 1_000_000.0) / elapsed.as_secs_f64()
        );
        println!();

        // Try a larger query with 10,000 rows (reduced for faster example execution)
        let large_row_count = 10_000;
        let large_query = format!(
            r"
            SELECT
                i as id,
                i % 1000 as bucket,
                random() as random_value
            FROM generate_series(1, {large_row_count}) as s(i)
            "
        );

        println!("Executing larger query to generate {large_row_count} rows...");
        let start = Instant::now();
        let result = conn.execute_query(&large_query)?;
        let elapsed = start.elapsed();

        let arrow_data = result.arrow_data();
        println!("Query completed in {elapsed:?}");
        println!("Result statistics:");
        println!("  Rows requested: {large_row_count}");
        println!(
            "  Arrow data size: {} bytes ({:.2} MB)",
            arrow_data.len(),
            arrow_data.len() as f64 / 1_000_000.0
        );
        println!(
            "  Bytes per row: {:.2}",
            arrow_data.len() as f64 / f64::from(large_row_count)
        );
        println!(
            "  Throughput: {:.2} MB/s",
            (arrow_data.len() as f64 / 1_000_000.0) / elapsed.as_secs_f64()
        );
        println!();

        Ok(())
    }

    #[expect(
        clippy::similar_names,
        reason = "paired bindings (request/response, reader/writer, etc.) are more readable with symmetric names than artificially distinct ones"
    )]
    /// Demonstrates different transfer modes
    pub(crate) fn transfer_mode_comparison() -> Result<()> {
        println!("=== Transfer Mode Comparison ===\n");

        // Start Hyper
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        params.set_listen_mode(ListenMode::Grpc { port: 0 });
        let hyper = HyperProcess::new(None, Some(&params))?;
        let grpc_url = hyper.grpc_url().unwrap();

        let row_count = 5_000;
        let query = format!(
            "SELECT i, i*2, 'text_' || CAST(i AS TEXT) FROM generate_series(1, {row_count}) as s(i)"
        );

        // Test SYNC mode
        println!("Testing SYNC mode ({row_count} rows)...");
        let config_sync = GrpcConfig::new(&grpc_url).transfer_mode(TransferMode::Sync);
        if let Ok(mut conn) = GrpcConnection::connect_with_config(config_sync) {
            let start = Instant::now();
            match conn.execute_query(&query) {
                Ok(result) => {
                    println!(
                        "  SYNC: {} bytes in {:?}",
                        result.arrow_data().len(),
                        start.elapsed()
                    );
                }
                Err(e) => println!("  SYNC failed: {e}"),
            }
        }

        // Test ADAPTIVE mode (default)
        println!("Testing ADAPTIVE mode ({row_count} rows)...");
        let config_adaptive = GrpcConfig::new(&grpc_url).transfer_mode(TransferMode::Adaptive);
        if let Ok(mut conn) = GrpcConnection::connect_with_config(config_adaptive) {
            let start = Instant::now();
            match conn.execute_query(&query) {
                Ok(result) => {
                    println!(
                        "  ADAPTIVE: {} bytes in {:?}",
                        result.arrow_data().len(),
                        start.elapsed()
                    );
                }
                Err(e) => println!("  ADAPTIVE failed: {e}"),
            }
        }

        // Test ASYNC mode
        println!("Testing ASYNC mode ({row_count} rows)...");
        let config_async = GrpcConfig::new(&grpc_url).transfer_mode(TransferMode::Async);
        if let Ok(mut conn) = GrpcConnection::connect_with_config(config_async) {
            let start = Instant::now();
            match conn.execute_query(&query) {
                Ok(result) => {
                    println!(
                        "  ASYNC: {} bytes in {:?}",
                        result.arrow_data().len(),
                        start.elapsed()
                    );
                }
                Err(e) => println!("  ASYNC failed: {e}"),
            }
        }

        println!("\nRecommendation: Use ADAPTIVE (default) for most workloads.");
        println!("- SYNC: Simple queries with small results (<100s execution time)");
        println!("- ADAPTIVE: Best balance of latency and reliability");
        println!("- ASYNC: Very large results or long-running queries\n");

        Ok(())
    }

    /// Demonstrates async gRPC usage
    pub(crate) async fn async_example() -> Result<()> {
        println!("=== Asynchronous gRPC Example ===\n");

        // Start Hyper
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        params.set_listen_mode(ListenMode::Grpc { port: 0 });
        let hyper = HyperProcess::new(None, Some(&params))?;
        let grpc_url = hyper.grpc_url().unwrap();

        println!("Connecting to Hyper via gRPC (async)...");
        let config = GrpcConfig::new(&grpc_url);
        let mut conn = GrpcConnectionAsync::connect_with_config(config).await?;

        // Execute a query asynchronously
        let arrow_data = conn
            .execute_query_to_arrow("SELECT 'async' as mode, CURRENT_TIMESTAMP as ts")
            .await?;

        println!(
            "Async query returned {} bytes of Arrow IPC data\n",
            arrow_data.len()
        );

        // Close the connection
        conn.close().await?;

        Ok(())
    }

    /// Demonstrates custom configuration options
    pub(crate) fn custom_config_example() -> Result<()> {
        println!("=== Custom Configuration Example ===\n");

        // Start Hyper
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        params.set_listen_mode(ListenMode::Grpc { port: 0 });
        let hyper = HyperProcess::new(None, Some(&params))?;
        let grpc_url = hyper.grpc_url().unwrap();

        // Build a custom configuration
        let config = GrpcConfig::new(&grpc_url)
            .connect_timeout(Duration::from_secs(10))
            .request_timeout(Duration::from_secs(60))
            .transfer_mode(TransferMode::Adaptive)
            .header("x-custom-header", "my-value");

        println!("Configuration:");
        println!("  Endpoint: {}", config.endpoint());
        println!("  TLS: {}", config.is_tls());

        let mut conn = GrpcConnection::connect_with_config(config)?;
        let result = conn.execute_query("SELECT 42 as answer")?;
        println!("Query succeeded: {} bytes\n", result.arrow_data().len());

        Ok(())
    }

    /// Demonstrates Arrow data processing hints
    pub(crate) fn arrow_processing_example() -> Result<()> {
        println!("=== Arrow Processing Example ===\n");

        // Start Hyper
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        params.set_listen_mode(ListenMode::Grpc { port: 0 });
        let hyper = HyperProcess::new(None, Some(&params))?;
        let grpc_url = hyper.grpc_url().unwrap();

        let config = GrpcConfig::new(&grpc_url);
        let mut conn = GrpcConnection::connect_with_config(config)?;

        // Execute a query
        let arrow_data = conn
            .execute_query_to_arrow("SELECT 1 as id, 'Alice' as name UNION ALL SELECT 2, 'Bob'")?;

        println!("Arrow IPC data received: {} bytes", arrow_data.len());
        println!("\nTo process this data, use the arrow crate:");
        println!("```rust");
        println!("use arrow::ipc::reader::StreamReader;");
        println!("use std::io::Cursor;");
        println!();
        println!("let reader = StreamReader::try_new(Cursor::new(&arrow_data), None)?;");
        println!("for batch in reader {{");
        println!("    let batch = batch?;");
        println!("    println!(\"Batch has {{}} rows\", batch.num_rows());");
        println!("}}");
        println!("```\n");

        Ok(())
    }

    /// Demonstrates reading, outputting, and aggregating Arrow results
    pub(crate) fn arrow_reading_and_aggregation_example() -> Result<()> {
        use arrow::array::{Array, Float64Array, Int64Array, StringArray};
        use arrow::ipc::reader::StreamReader;
        use std::io::Cursor;

        println!("=== Arrow Reading & Aggregation Example ===\n");

        // Start Hyper with gRPC
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        params.set_listen_mode(ListenMode::Grpc { port: 0 });
        let hyper = HyperProcess::new(None, Some(&params))?;
        let grpc_url = hyper.grpc_url().unwrap();

        let config = GrpcConfig::new(&grpc_url);
        let mut conn = GrpcConnection::connect_with_config(config)?;

        // ===== Example 1: Simple data with names =====
        println!("--- Example 1: Reading Simple Data ---\n");

        // Use explicit casts to ensure consistent types
        let query = r"
            SELECT CAST(1 AS BIGINT) as id, 'Alice' as name, CAST(28 AS BIGINT) as age, CAST(55000.0 AS DOUBLE PRECISION) as salary
            UNION ALL SELECT 2, 'Bob', 35, 72000.0
            UNION ALL SELECT 3, 'Charlie', 42, 85000.0
            UNION ALL SELECT 4, 'Diana', 31, 68000.0
            UNION ALL SELECT 5, 'Eve', 26, 52000.0
        ";

        let arrow_data = conn.execute_query_to_arrow(query)?;
        let reader = StreamReader::try_new(Cursor::new(&arrow_data), None)
            .map_err(|e| hyperdb_api::Error::conversion(format!("Arrow error: {e}")))?;

        println!("Schema:");
        for field in reader.schema().fields() {
            println!("  - {} ({})", field.name(), field.data_type());
        }
        println!();

        // Read and print all rows
        println!("Data:");
        println!("{:>4} {:>10} {:>5} {:>12}", "ID", "Name", "Age", "Salary");
        println!("{}", "-".repeat(35));

        for batch_result in reader {
            let batch = batch_result
                .map_err(|e| hyperdb_api::Error::conversion(format!("Arrow error: {e}")))?;

            // Get columns by index (matching the SELECT order)
            let id_col = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            let name_col = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let age_col = batch
                .column(2)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            let salary_col = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();

            for i in 0..batch.num_rows() {
                println!(
                    "{:>4} {:>10} {:>5} {:>12.2}",
                    id_col.value(i),
                    name_col.value(i),
                    age_col.value(i),
                    salary_col.value(i)
                );
            }
        }
        println!();

        // ===== Example 2: Aggregation on larger dataset =====
        println!("--- Example 2: Manual Aggregation ---\n");

        let row_count = 1_000;
        let query = format!(
            r"
            SELECT
                CAST(i AS BIGINT) as id,
                CAST(i % 5 AS BIGINT) as category,
                CAST(i * 1.5 AS DOUBLE PRECISION) as value,
                CAST(random() * 100 AS DOUBLE PRECISION) as random_value
            FROM generate_series(1, {row_count}) as s(i)
            "
        );

        let arrow_data = conn.execute_query_to_arrow(&query)?;
        let reader = StreamReader::try_new(Cursor::new(&arrow_data), None)
            .map_err(|e| hyperdb_api::Error::conversion(format!("Arrow error: {e}")))?;

        // Aggregate statistics
        let mut total_rows: u64 = 0;
        let mut sum_value: f64 = 0.0;
        let mut sum_random: f64 = 0.0;
        let mut min_random: f64 = f64::MAX;
        let mut max_random: f64 = f64::MIN;
        let mut category_counts: [u64; 5] = [0; 5];
        let mut category_sums: [f64; 5] = [0.0; 5];

        for batch_result in reader {
            let batch = batch_result
                .map_err(|e| hyperdb_api::Error::conversion(format!("Arrow error: {e}")))?;

            let category_col = batch
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            let value_col = batch
                .column(2)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            let random_col = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();

            for i in 0..batch.num_rows() {
                total_rows += 1;
                let category = category_col.value(i) as usize;
                let value = value_col.value(i);
                let random = random_col.value(i);

                sum_value += value;
                sum_random += random;
                min_random = min_random.min(random);
                max_random = max_random.max(random);
                category_counts[category] += 1;
                category_sums[category] += value;
            }
        }

        println!("Aggregation Results ({total_rows} rows):");
        println!();
        println!("Overall Statistics:");
        println!("  Total rows:    {total_rows}");
        println!("  Sum of value:  {sum_value:.2}");
        println!("  Avg value:     {:.2}", sum_value / total_rows as f64);
        println!("  Sum of random: {sum_random:.2}");
        println!("  Avg random:    {:.2}", sum_random / total_rows as f64);
        println!("  Min random:    {min_random:.4}");
        println!("  Max random:    {max_random:.4}");
        println!();
        println!("Category Breakdown:");
        println!(
            "{:>8} {:>10} {:>15} {:>12}",
            "Category", "Count", "Sum", "Avg"
        );
        println!("{}", "-".repeat(50));
        for cat in 0..5 {
            let count = category_counts[cat];
            let sum = category_sums[cat];
            let avg = if count > 0 { sum / count as f64 } else { 0.0 };
            println!("{cat:>8} {count:>10} {sum:>15.2} {avg:>12.2}");
        }
        println!();

        // ===== Example 3: Server-side aggregation =====
        println!("--- Example 3: Server-side Aggregation (SQL) ---\n");

        let agg_query = format!(
            r"
            SELECT
                CAST(category AS BIGINT) as category,
                CAST(COUNT(*) AS BIGINT) as count,
                CAST(SUM(value) AS DOUBLE PRECISION) as sum_value,
                CAST(AVG(value) AS DOUBLE PRECISION) as avg_value,
                CAST(MIN(random_value) AS DOUBLE PRECISION) as min_random,
                CAST(MAX(random_value) AS DOUBLE PRECISION) as max_random
            FROM (
                SELECT
                    i % 5 as category,
                    CAST(i * 1.5 AS DOUBLE PRECISION) as value,
                    CAST(random() * 100 AS DOUBLE PRECISION) as random_value
                FROM generate_series(1, {row_count}) as s(i)
            ) t
            GROUP BY category
            ORDER BY category
            "
        );

        let arrow_data = conn.execute_query_to_arrow(&agg_query)?;
        let reader = StreamReader::try_new(Cursor::new(&arrow_data), None)
            .map_err(|e| hyperdb_api::Error::conversion(format!("Arrow error: {e}")))?;

        println!("Server-side aggregation results:");
        println!(
            "{:>8} {:>10} {:>15} {:>12} {:>12} {:>12}",
            "Category", "Count", "Sum", "Avg", "Min Rand", "Max Rand"
        );
        println!("{}", "-".repeat(75));

        for batch_result in reader {
            let batch = batch_result
                .map_err(|e| hyperdb_api::Error::conversion(format!("Arrow error: {e}")))?;

            let cat_col = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            let count_col = batch
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            let sum_col = batch
                .column(2)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            let avg_col = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            let min_col = batch
                .column(4)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();
            let max_col = batch
                .column(5)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap();

            for i in 0..batch.num_rows() {
                println!(
                    "{:>8} {:>10} {:>15.2} {:>12.2} {:>12.4} {:>12.4}",
                    cat_col.value(i),
                    count_col.value(i),
                    sum_col.value(i),
                    avg_col.value(i),
                    min_col.value(i),
                    max_col.value(i)
                );
            }
        }
        println!();

        Ok(())
    }

    /// Demonstrates SQL queries with gRPC connections
    pub(crate) fn query_builder_with_grpc() -> Result<()> {
        println!("=== SQL Queries with gRPC ===\n");

        // Start Hyper with gRPC
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        params.set_listen_mode(ListenMode::Grpc { port: 0 });
        let hyper = HyperProcess::new(None, Some(&params))?;
        let grpc_url = hyper.grpc_url().unwrap();

        println!("Hyper gRPC endpoint: {}\n", hyper.grpc_endpoint().unwrap());

        let config = GrpcConfig::new(&grpc_url);
        let mut conn = GrpcConnection::connect_with_config(config)?;

        // Test 1: Subquery FROM clause
        println!("1. Subquery FROM clause with WHERE/ORDER BY");
        let query = "SELECT id, name FROM (SELECT i as id, 'Name_' || CAST(i AS TEXT) as name FROM generate_series(1, 5) as s(i)) sub WHERE id > 2 ORDER BY id";
        println!("   SQL: {query}");
        let result = conn.execute_query(query)?;
        println!(
            "   Result: {} bytes of Arrow data\n",
            result.arrow_data().len()
        );

        // Test 2: WHERE IN clause
        println!("2. WHERE IN clause");
        let query = "SELECT id, bucket FROM (SELECT i as id, i % 10 as bucket FROM generate_series(1, 100) as s(i)) sub WHERE bucket IN (1, 2, 3) LIMIT 10";
        println!("   SQL: {query}");
        let result = conn.execute_query(query)?;
        println!(
            "   Result: {} bytes of Arrow data\n",
            result.arrow_data().len()
        );

        // Test 3: Aggregation with gRPC
        println!("3. Aggregation query");
        let agg_query =
            "SELECT COUNT(*) as cnt, SUM(i) as total FROM generate_series(1, 100) as s(i)";
        println!("   SQL: {agg_query}");
        let result = conn.execute_query(agg_query)?;
        println!(
            "   Result: {} bytes of Arrow data\n",
            result.arrow_data().len()
        );

        // Test 4: Execute query to Arrow format directly
        println!("4. Direct Arrow IPC output");
        let arrow_data = conn.execute_query_to_arrow(
            "SELECT i as value, i * 2 as doubled FROM generate_series(1, 10) as s(i)",
        )?;
        println!("   Arrow IPC data size: {} bytes\n", arrow_data.len());

        // Note about gRPC limitations
        println!("Note: gRPC connections are read-only.");
        println!("      Write operations require a TCP connection.\n");

        Ok(())
    }
}

fn main() -> hyperdb_api::Result<()> {
    use grpc_example::{
        arrow_processing_example, arrow_reading_and_aggregation_example, async_example,
        both_modes_example, custom_config_example, grpc_with_hyper_process, large_query_example,
        query_builder_with_grpc, transfer_mode_comparison,
    };

    println!("╔════════════════════════════════════════════════════════════════╗");
    println!("║           Hyper gRPC Connection Example                        ║");
    println!("╚════════════════════════════════════════════════════════════════╝\n");

    // Run examples that start their own HyperProcess
    grpc_with_hyper_process()?;
    both_modes_example()?;
    query_builder_with_grpc()?;
    custom_config_example()?;
    arrow_processing_example()?;
    arrow_reading_and_aggregation_example()?;
    large_query_example()?;
    transfer_mode_comparison()?;

    // Run async example with tokio
    println!("Starting async runtime...\n");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime")
        .block_on(async_example())?;

    println!("All examples completed!");
    Ok(())
}
