// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Comprehensive async integration example for Hyper database.
//!
//! This example demonstrates:
//! - `AsyncConnection` API for native async operations
//! - Integrating the synchronous Hyper API with Tokio using `spawn_blocking`
//! - Running concurrent database operations
//! - Best practices for async database access
//!
//! # Running this example
//!
//! Requires the pinned `hyperd`: run `make download-hyperd` once, or set an
//! absolute `HYPERD_PATH`.
//!
//! ```bash
//! cargo run -p hyperdb-api --example async_usage
//! ```

#![allow(clippy::cast_precision_loss, reason = "example throughput display")]

use std::sync::Arc;

use hyperdb_api::{
    AsyncConnection, Catalog, Connection, CreateMode, HyperProcess, Inserter, Parameters, Result,
    Row, SqlType, TableDefinition,
};

// =============================================================================
// User struct for clean data representation
// =============================================================================

#[derive(Debug, Clone)]
struct User {
    id: i64,
    name: String,
    email: Option<String>,
    balance: f64,
}

impl User {
    fn from_row(row: &Row) -> Self {
        Self {
            id: row.get(0).unwrap_or(0),
            name: row.get(1).unwrap_or_default(),
            email: row.get(2),
            balance: row.get(3).unwrap_or(0.0),
        }
    }
}

// =============================================================================
// Helper functions for database operations
// =============================================================================

fn users_table() -> TableDefinition {
    TableDefinition::new("users")
        .add_required_column("id", SqlType::big_int())
        .add_required_column("name", SqlType::text())
        .add_nullable_column("email", SqlType::text())
        .add_required_column("balance", SqlType::double())
}

fn fetch_high_value_users(connection: &Connection, min_balance: f64) -> Result<Vec<User>> {
    let mut result = connection.query_params(
        "SELECT id, name, email, balance FROM users WHERE balance > $1",
        &[&min_balance],
    )?;

    let mut users = Vec::new();
    while let Some(chunk) = result.next_chunk()? {
        users.extend(chunk.iter().map(User::from_row));
    }

    Ok(users)
}

fn get_total_balance(connection: &Connection) -> Result<f64> {
    let total: Option<f64> = connection.execute_scalar_query("SELECT SUM(balance) FROM users")?;
    Ok(total.unwrap_or(0.0))
}

fn get_user_count(connection: &Connection) -> Result<i64> {
    let count: Option<i64> = connection.execute_scalar_query("SELECT COUNT(*) FROM users")?;
    Ok(count.unwrap_or(0))
}

// =============================================================================
// Main entry point
// =============================================================================

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║          Comprehensive Async Integration Example              ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    // Create test_results directory
    std::fs::create_dir_all("test_results").ok();

    // =========================================================================
    // Part 1: AsyncConnection API (Native Async)
    // =========================================================================
    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║  Part 1: AsyncConnection API                                  ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    demonstrate_async_connection().await?;

    // =========================================================================
    // Part 2: Tokio Integration with spawn_blocking
    // =========================================================================
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║  Part 2: Tokio Integration (spawn_blocking)                   ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    demonstrate_spawn_blocking().await?;

    // =========================================================================
    // Best Practices Summary
    // =========================================================================
    print_best_practices();

    Ok(())
}

// =============================================================================
// Part 1: AsyncConnection API
// =============================================================================

async fn demonstrate_async_connection()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize logging (optional)
    // tracing_subscriber::fmt()
    //     .with_env_filter("hyperdb_api=debug")
    //     .init();

    println!("Starting Hyper server...");
    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper.require_endpoint()?;
    println!("  Hyper started at: {endpoint}\n");

    // Create async connection
    let db_path = "test_results/async_connection.hyper";
    let conn = AsyncConnection::connect(endpoint, db_path, CreateMode::CreateAndReplace).await?;

    println!("Connected via {}\n", conn.transport_type());

    // Create a table
    conn.execute_command(
        "CREATE TABLE users (
            id INT NOT NULL,
            name TEXT NOT NULL,
            email TEXT,
            created_at TIMESTAMP DEFAULT NOW()
        )",
    )
    .await?;
    println!("✓ Created users table");

    // Insert data
    let affected = conn
        .execute_command(
            "INSERT INTO users (id, name, email) VALUES (1, 'Alice', 'alice@example.com')",
        )
        .await?;
    println!("✓ Inserted {affected} row(s)");

    conn.execute_command(
        "INSERT INTO users (id, name, email) VALUES (2, 'Bob', 'bob@example.com')",
    )
    .await?;
    conn.execute_command("INSERT INTO users (id, name) VALUES (3, 'Charlie')")
        .await?;
    println!("✓ Inserted additional rows\n");

    println!("AsyncConnection is working!");
    println!("Note: Full query support with execute_query method available.\n");

    // Close connection
    conn.close().await?;
    println!("✓ Connection closed");

    Ok(())
}

// =============================================================================
// Part 2: Tokio Integration with spawn_blocking
// =============================================================================

async fn demonstrate_spawn_blocking()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Setup database
    println!("Setting up database (blocking operation)...");

    let hyper = tokio::task::spawn_blocking(|| -> Result<Arc<HyperProcess>> {
        let mut params = Parameters::new();
        params.set("log_dir", "test_results");
        Ok(Arc::new(HyperProcess::new(None, Some(&params))?))
    })
    .await??;

    let hyper_setup = Arc::clone(&hyper);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let connection = Connection::new(
            &hyper_setup,
            "test_results/async_tokio.hyper",
            CreateMode::CreateAndReplace,
        )?;
        let catalog = Catalog::new(&connection);

        let users_def = users_table();
        catalog.create_table(&users_def)?;

        {
            let mut inserter = Inserter::new(&connection, &users_def)?;
            inserter.add_row(&[&1i64, &"Alice", &Some("alice@example.com"), &1500.50f64])?;
            inserter.add_row(&[&2i64, &"Bob", &Some("bob@example.com"), &2300.00f64])?;
            inserter.add_row(&[&3i64, &"Charlie", &None::<&str>, &890.25f64])?;
            inserter.add_row(&[&4i64, &"Diana", &Some("diana@example.com"), &3100.00f64])?;
            inserter.add_row(&[&5i64, &"Eve", &Some("eve@example.com"), &1750.75f64])?;
            inserter.execute()?;
        }

        println!("✓ Database setup complete: 5 users inserted\n");
        Ok(())
    })
    .await??;

    // -------------------------------------------------------------------------
    // Example 1: Single async database query
    // -------------------------------------------------------------------------
    println!("--- Example 1: Single Async Query ---\n");

    let hyper_query = Arc::clone(&hyper);
    let high_value_users = tokio::task::spawn_blocking(move || -> Result<Vec<User>> {
        let connection = Connection::new(
            &hyper_query,
            "test_results/async_tokio.hyper",
            CreateMode::DoNotCreate,
        )?;
        fetch_high_value_users(&connection, 2000.0)
    })
    .await??;

    println!("High-value users (balance > $2000):");
    for user in &high_value_users {
        println!(
            "  [ID: {}] {} ({}) - ${:.2}",
            user.id,
            user.name,
            user.email.as_deref().unwrap_or("No Email"),
            user.balance
        );
    }

    // -------------------------------------------------------------------------
    // Example 2: Concurrent queries
    // -------------------------------------------------------------------------
    println!("\n--- Example 2: Concurrent Queries ---\n");
    println!("Running multiple queries concurrently with tokio::join!...\n");

    let hyper1 = Arc::clone(&hyper);
    let hyper2 = Arc::clone(&hyper);
    let hyper3 = Arc::clone(&hyper);

    let task_users = tokio::task::spawn_blocking(move || -> Result<Vec<User>> {
        let connection = Connection::new(
            &hyper1,
            "test_results/async_tokio.hyper",
            CreateMode::DoNotCreate,
        )?;
        println!("  [Task 1] Fetching all users...");
        let mut result = connection.execute_query("SELECT id, name, email, balance FROM users")?;
        let mut users = Vec::new();
        while let Some(chunk) = result.next_chunk()? {
            users.extend(chunk.iter().map(User::from_row));
        }
        println!("  [Task 1] Done: {} users found", users.len());
        Ok(users)
    });

    let task_total = tokio::task::spawn_blocking(move || -> Result<f64> {
        let connection = Connection::new(
            &hyper2,
            "test_results/async_tokio.hyper",
            CreateMode::DoNotCreate,
        )?;
        println!("  [Task 2] Calculating total balance...");
        let total = get_total_balance(&connection)?;
        println!("  [Task 2] Done: ${total:.2}");
        Ok(total)
    });

    let task_count = tokio::task::spawn_blocking(move || -> Result<i64> {
        let connection = Connection::new(
            &hyper3,
            "test_results/async_tokio.hyper",
            CreateMode::DoNotCreate,
        )?;
        println!("  [Task 3] Counting users...");
        let count = get_user_count(&connection)?;
        println!("  [Task 3] Done: {count} users");
        Ok(count)
    });

    let (users_result, total_result, count_result) =
        tokio::join!(task_users, task_total, task_count);

    let all_users = users_result??;
    let total_balance = total_result??;
    let user_count = count_result??;

    println!("\nCombined Results:");
    println!("  Total users: {user_count}");
    println!("  Total balance: ${total_balance:.2}");
    println!(
        "  Average balance: ${:.2}",
        total_balance / user_count as f64
    );

    // -------------------------------------------------------------------------
    // Example 3: Async processing of query results
    // -------------------------------------------------------------------------
    println!("\n--- Example 3: Async Processing of Results ---\n");

    for user in &all_users {
        // Simulate async operations (HTTP calls, file I/O, etc.)
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        println!(
            "  Processed user: {} (simulated async operation)",
            user.name
        );
    }

    Ok(())
}

// =============================================================================
// Best Practices Summary
// =============================================================================

fn print_best_practices() {
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║  Best Practices for Async Integration                         ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    println!("1. Use AsyncConnection when available");
    println!("   For native async operations without blocking the runtime.\n");

    println!("2. Use spawn_blocking for synchronous Connection");
    println!("   Wrap all blocking database calls to avoid blocking the executor.\n");

    println!("3. Give each blocking task its own Connection");
    println!("   A Connection runs one statement at a time; concurrent tasks need separate");
    println!("   connections (or a pool::ConnectionPool).\n");

    println!("4. Share HyperProcess with Arc");
    println!("   HyperProcess can be wrapped in Arc and shared across tasks.\n");

    println!("5. Use tokio::join! for concurrent queries");
    println!("   Spawn separate blocking tasks and join them for parallelism.\n");

    println!("6. Process results asynchronously");
    println!("   Once data is retrieved, process it with async code (HTTP, file I/O).\n");

    println!("7. Consider connection pooling for high load");
    println!("   See the connection_pool example for async connection pooling.\n");
}
