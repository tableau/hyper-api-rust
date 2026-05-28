// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Connection pool example demonstrating async connection pooling with deadpool.
//!
//! This example shows how to use the connection pool for efficient connection
//! reuse in async applications with multiple concurrent tasks.
//!
//! Run with: cargo run --example `connection_pool` --features pool

use std::time::Duration;

use hyperdb_api::pool::{create_pool, PoolConfig};
use hyperdb_api::{CreateMode, HyperProcess, Result};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter("hyperdb_api=debug")
        .init();

    println!("=== Connection Pool Example ===\n");

    // Create test_results directory if it doesn't exist
    std::fs::create_dir_all("test_results")?;

    // Start a local Hyper server with logs in test_results
    use hyperdb_api::Parameters;
    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper.require_endpoint()?.to_string();
    println!("Started Hyper server at: {endpoint}");

    // Create a pool configuration
    let config = PoolConfig::new(&endpoint, "test_results/connection_pool.hyper")
        .create_mode(CreateMode::CreateAndReplace)
        .max_size(4); // Small pool for demo

    // Build the pool
    let pool = create_pool(config)?;
    println!("Created connection pool with max_size=4");

    // Get a connection to set up the schema
    {
        let conn = pool
            .get()
            .await
            .map_err(|e| hyperdb_api::Error::internal(e.to_string()))?;
        conn.execute_command(
            "CREATE TABLE counters (
                id INT NOT NULL,
                name TEXT NOT NULL,
                value INT DEFAULT 0
            )",
        )
        .await?;
        println!("Created counters table");

        // Insert initial data
        for i in 1..=10 {
            conn.execute_command(&format!(
                "INSERT INTO counters (id, name, value) VALUES ({i}, 'counter_{i}', 0)"
            ))
            .await?;
        }
        println!("Inserted 10 counter rows");
    }
    // Connection returned to pool

    // Spawn multiple concurrent tasks that use the pool
    println!("\nSpawning 8 concurrent tasks...");
    let mut handles = Vec::new();

    for task_id in 0..8 {
        let pool = pool.clone();
        let handle = tokio::spawn(async move {
            // Get a connection from the pool
            let conn = pool.get().await.expect("Failed to get connection");

            // Simulate some work
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Update a counter
            let counter_id = (task_id % 10) + 1;
            conn.execute_command(&format!(
                "UPDATE counters SET value = value + 1 WHERE id = {counter_id}"
            ))
            .await
            .expect("Failed to update counter");

            println!("Task {task_id} updated counter {counter_id}");
            // Connection returned to pool when dropped
        });
        handles.push(handle);
    }

    // Wait for all tasks to complete
    for handle in handles {
        handle.await.expect("Task panicked");
    }

    println!("\nAll tasks completed!");
    println!("Pool status: {} connections in use", pool.status().size);

    Ok(())
}
