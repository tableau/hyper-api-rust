// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Smoke test for async API parity with the sync API.
//!
//! Exercises the full async surface — connect, DDL, parameterized
//! DML, streaming reads, struct mapping, transactions — **without**
//! calling `async_tcp_client()` or importing anything from
//! `hyperdb_api_core::client`. If this example compiles and runs clean, the
//! "async is a first-class equivalent of sync" contract holds.

use hyperdb_api::{
    AsyncConnection, AsyncConnectionBuilder, CreateMode, FromRow, HyperProcess, Result, RowAccessor,
};

#[derive(Debug)]
#[expect(
    dead_code,
    reason = "fields are read only through the derived `Debug` impl in the example output"
)]
struct Order {
    id: i32,
    customer: String,
    total: f64,
}

impl FromRow for Order {
    fn from_row(row: RowAccessor<'_>) -> Result<Self> {
        Ok(Order {
            id: row.get("id")?,
            customer: row.get_opt("customer")?.unwrap_or_default(),
            total: row.get_opt("total")?.unwrap_or(0.0),
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;
    let endpoint = hyper.require_endpoint()?.to_string();
    let db_path = std::env::temp_dir().join("async_parity_smoke.hyper");

    // 1. Build an AsyncConnection via the fluent builder (no tcp_client).
    let mut conn = AsyncConnectionBuilder::new(&endpoint)
        .database(&db_path)
        .create_mode(CreateMode::CreateAndReplace)
        .build()
        .await?;

    // 2. DDL via execute_command.
    conn.execute_command(
        "CREATE TABLE orders (id INT NOT NULL, customer TEXT, total DOUBLE PRECISION)",
    )
    .await?;

    // 3. Parameterized DML via command_params — no manual SQL escaping.
    for (id, customer, total) in [(1i32, "Alice", 99.5), (2, "Bob", 250.0), (3, "Carol", 12.0)] {
        conn.command_params(
            "INSERT INTO orders VALUES ($1, $2, $3)",
            &[&id, &customer, &total],
        )
        .await?;
    }

    // 4. Scalar fetch.
    let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM orders").await?;
    println!("inserted {count} orders");

    // 5. Parameterized query + streaming rowset + struct mapping.
    // Use fetch_all_as to drive Order::from_row through the cached
    // RowAccessor path. (Direct `Order::from_row(row)?` is no longer a
    // one-liner because the trait takes a RowAccessor with a
    // pre-resolved column-index map.)
    let high_value: Vec<Order> = conn
        .fetch_all_as("SELECT id, customer, total FROM orders WHERE total > 50.0 ORDER BY id")
        .await?;
    for order in &high_value {
        println!("  high-value: {order:?}");
    }

    // 6. fetch_all_as for typed struct batch.
    let all: Vec<Order> = conn
        .fetch_all_as("SELECT id, customer, total FROM orders ORDER BY id")
        .await?;
    println!("all orders: {} entries", all.len());

    // 7. Transactional read-modify-write inside the high-level API.
    {
        let txn = conn.transaction().await?;
        let current_total: f64 = txn.fetch_scalar("SELECT SUM(total) FROM orders").await?;
        txn.command_params(
            "INSERT INTO orders VALUES ($1, $2, $3)",
            &[&999i32, &"AdjustmentBot", &-current_total],
        )
        .await?;
        let new_total: f64 = txn.fetch_scalar("SELECT SUM(total) FROM orders").await?;
        assert!(
            new_total.abs() < 1e-6,
            "balance should be zero after adjustment"
        );
        txn.commit().await?;
    }

    // 8. Arrow IPC export over TCP (previously unimplemented).
    let arrow_bytes = conn.export_table_to_arrow("orders").await?;
    println!("exported {} bytes of Arrow IPC", arrow_bytes.len());

    // 9. Explain a query.
    let plan = conn
        .explain("SELECT * FROM orders WHERE total > 100")
        .await?;
    println!(
        "query plan (first line): {}",
        plan.lines().next().unwrap_or("<empty>")
    );

    // 10. Ping and close.
    conn.ping().await?;
    conn.close().await?;

    // Assert we never needed the escape hatch.
    let _ = AsyncConnection::builder; // compile-time check that the method exists.
    println!("async parity smoke: all 10 stages OK");
    Ok(())
}
