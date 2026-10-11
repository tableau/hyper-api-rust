// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Example: Transactions
//!
//! Demonstrates the transaction API:
//! - **Recommended:** RAII `Transaction` guard with explicit commit and rollback
//! - Querying within transactions to see uncommitted data
//! - Multiple operations (INSERT, UPDATE, DELETE) in a single transaction
//! - Multi-table atomic rollback (referential integrity across tables)
//! - Multi-table reconnect semantics: only committed cross-table data is visible after reconnect
//! - Auto-rollback safety net when the guard is dropped without commit
//! - DDL inside transactions and known restrictions
//! - Unguarded transaction control (`begin_transaction_unguarded`,
//!   `commit_unguarded`, `rollback_unguarded`) for code that cannot take
//!   the guard's `&mut` borrow, such as a helper holding `&Connection`.
//!   Prefer the RAII guard everywhere else: you must pair every unguarded
//!   `BEGIN` with a commit or rollback yourself, on every path.
//!
//! Run with: `cargo run -p hyperdb-api --example transactions`
//!
//! Requires the pinned `hyperd`: run `make download-hyperd` once, or set an
//! absolute `HYPERD_PATH`.

use hyperdb_api::{
    Catalog, Connection, CreateMode, HyperProcess, Parameters, Result, SqlType, TableDefinition,
};

fn main() -> Result<()> {
    std::fs::create_dir_all("test_results")?;

    println!("Starting Hyper process...");
    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    let hyper = HyperProcess::new(None, Some(&params))?;

    let db_path = "test_results/transactions.hyper";
    let mut connection = Connection::new(&hyper, db_path, CreateMode::CreateAndReplace)?;
    println!("Created database: {db_path}\n");

    // Example 1: Unguarded transaction control
    example_raw_transaction(&connection)?;

    // Example 2: RAII Transaction guard
    example_transaction_guard(&mut connection)?;

    // Example 3: Querying within a transaction
    example_query_within_transaction(&mut connection)?;

    // Example 4: Multiple operations in one transaction
    example_multiple_operations(&mut connection)?;

    // Example 5: Multi-table atomic rollback
    example_multi_table_rollback(&mut connection)?;

    // Example 6: Multi-table reconnect semantics
    example_multi_table_reconnect(&hyper)?;

    // Example 7: Auto-rollback on drop (the safety net)
    example_auto_rollback_on_drop(&mut connection)?;

    // Example 8: DDL in transactions and known restrictions
    // (Last because it deliberately triggers a server error inside a transaction.)
    example_ddl_in_transactions(&mut connection)?;

    println!("\nAll transaction examples completed successfully!");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
fn create_accounts_table(connection: &Connection, table_name: &str) -> Result<()> {
    let catalog = Catalog::new(connection);
    let def = TableDefinition::from(table_name)
        .add_required_column("id", SqlType::int())
        .add_required_column("name", SqlType::text())
        .add_required_column("balance", SqlType::double());
    catalog.create_table(&def)?;
    Ok(())
}

fn row_count(connection: &Connection, table_name: &str) -> Result<i64> {
    connection.query_count(&format!("SELECT COUNT(*) FROM {table_name}"))
}

fn print_table(connection: &Connection, table_name: &str) -> Result<()> {
    let rows = connection.fetch_all(format!(
        "SELECT id, name, balance FROM {table_name} ORDER BY id"
    ))?;
    for row in &rows {
        let id: i32 = row.get(0).unwrap();
        let name: String = row.get(1).unwrap();
        let balance: f64 = row.get(2).unwrap();
        println!("    id={id}, name={name}, balance={balance:.2}");
    }
    if rows.is_empty() {
        println!("    (empty)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Example 1: Unguarded transaction control (&self helpers)
// ---------------------------------------------------------------------------
fn example_raw_transaction(connection: &Connection) -> Result<()> {
    println!("=== Example 1: Unguarded Transaction Control ===");

    create_accounts_table(connection, "raw_txn")?;

    // --- Commit path ---
    connection.begin_transaction_unguarded()?;
    connection.execute_command("INSERT INTO raw_txn VALUES (1, 'Alice', 1000.0)")?;
    connection.execute_command("INSERT INTO raw_txn VALUES (2, 'Bob',   500.0)")?;
    connection.commit_unguarded()?;
    println!("  After COMMIT: {} rows", row_count(connection, "raw_txn")?);

    // --- Rollback path ---
    connection.begin_transaction_unguarded()?;
    connection.execute_command("INSERT INTO raw_txn VALUES (3, 'Eve', 9999.0)")?;
    // Oops — roll it back
    connection.rollback_unguarded()?;
    println!(
        "  After ROLLBACK: {} rows (Eve's insert was undone)",
        row_count(connection, "raw_txn")?
    );

    print_table(connection, "raw_txn")?;
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Example 2: RAII Transaction guard — auto-rollback on drop
// ---------------------------------------------------------------------------
fn example_transaction_guard(connection: &mut Connection) -> Result<()> {
    println!("=== Example 2: RAII Transaction Guard ===");

    create_accounts_table(connection, "guard_txn")?;

    // Use the guard — commit explicitly
    {
        let txn = connection.transaction()?;
        txn.execute_command("INSERT INTO guard_txn VALUES (1, 'Alice', 1000.0)")?;
        txn.execute_command("INSERT INTO guard_txn VALUES (2, 'Bob',   500.0)")?;
        txn.commit()?;
        println!("  Committed via guard: 2 rows inserted");
    }

    // Use the guard — explicit rollback
    {
        let txn = connection.transaction()?;
        txn.execute_command("INSERT INTO guard_txn VALUES (3, 'Charlie', 750.0)")?;
        txn.rollback()?;
        println!("  Rolled back via guard: Charlie's insert undone");
    }

    println!("  Final row count: {}", row_count(connection, "guard_txn")?);
    print_table(connection, "guard_txn")?;
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Example 3: Querying within a transaction sees uncommitted data
// ---------------------------------------------------------------------------
fn example_query_within_transaction(connection: &mut Connection) -> Result<()> {
    println!("=== Example 3: Querying Within a Transaction ===");

    create_accounts_table(connection, "query_txn")?;

    // Seed one row outside any transaction
    connection.execute_command("INSERT INTO query_txn VALUES (1, 'Alice', 1000.0)")?;

    let txn = connection.transaction()?;
    txn.execute_command("INSERT INTO query_txn VALUES (2, 'Bob', 500.0)")?;

    // Query inside the transaction — both rows are visible
    let count_inside: i64 = txn.query_count("SELECT COUNT(*) FROM query_txn")?;
    println!("  Inside transaction: {count_inside} rows visible");

    // fetch_scalar to get a sum
    let total: f64 = txn.fetch_scalar("SELECT SUM(balance) FROM query_txn")?;
    println!("  Total balance inside transaction: {total:.2}");

    // fetch_one for a specific row
    let row = txn.fetch_one("SELECT name, balance FROM query_txn WHERE id = 2")?;
    let name: String = row.get(0).unwrap();
    let balance: f64 = row.get(1).unwrap();
    println!("  Bob's balance (uncommitted): {name} = {balance:.2}");

    txn.commit()?;
    println!(
        "  After commit: {} rows",
        row_count(connection, "query_txn")?
    );
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Example 4: Multiple operations (INSERT, UPDATE, DELETE) in one transaction
// ---------------------------------------------------------------------------
fn example_multiple_operations(connection: &mut Connection) -> Result<()> {
    println!("=== Example 4: Multiple Operations in One Transaction ===");

    create_accounts_table(connection, "multi_ops")?;

    let txn = connection.transaction()?;

    // Insert several rows
    txn.execute_command("INSERT INTO multi_ops VALUES (1, 'Alice',   1000.0)")?;
    txn.execute_command("INSERT INTO multi_ops VALUES (2, 'Bob',      500.0)")?;
    txn.execute_command("INSERT INTO multi_ops VALUES (3, 'Charlie',  750.0)")?;
    txn.execute_command("INSERT INTO multi_ops VALUES (4, 'Diana',    300.0)")?;
    println!("  Inserted 4 rows");

    // Transfer $200 from Alice to Bob
    txn.execute_command("UPDATE multi_ops SET balance = balance - 200 WHERE id = 1")?;
    txn.execute_command("UPDATE multi_ops SET balance = balance + 200 WHERE id = 2")?;
    println!("  Transferred $200 from Alice to Bob");

    // Remove Diana's account
    txn.execute_command("DELETE FROM multi_ops WHERE id = 4")?;
    println!("  Deleted Diana's account");

    txn.commit()?;
    println!("  Committed. Final state:");
    print_table(connection, "multi_ops")?;
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Example 5: Multi-table atomic rollback
//
// A single transaction spanning multiple related tables is fully atomic:
// either ALL tables are updated, or NONE are. This is the foundation of
// referential integrity in transactional systems.
// ---------------------------------------------------------------------------
fn example_multi_table_rollback(connection: &mut Connection) -> Result<()> {
    println!("=== Example 5: Multi-Table Atomic Rollback ===");

    let catalog = Catalog::new(connection);

    // Create related tables
    let customers_def = TableDefinition::from("mt_customers")
        .add_required_column("customer_id", SqlType::int())
        .add_required_column("name", SqlType::text())
        .add_required_column("email", SqlType::text());
    catalog.create_table(&customers_def)?;

    let orders_def = TableDefinition::from("mt_orders")
        .add_required_column("order_id", SqlType::int())
        .add_required_column("customer_id", SqlType::int())
        .add_required_column("product", SqlType::text())
        .add_required_column("amount", SqlType::double());
    catalog.create_table(&orders_def)?;

    // --- Committed transaction: insert a customer and their orders ---
    {
        let txn = connection.transaction()?;
        txn.execute_command("INSERT INTO mt_customers VALUES (1, 'Alice', 'alice@example.com')")?;
        txn.execute_command("INSERT INTO mt_orders VALUES (101, 1, 'Widget A', 29.99)")?;
        txn.execute_command("INSERT INTO mt_orders VALUES (102, 1, 'Widget B', 49.99)")?;
        txn.commit()?;
        println!("  Committed: Alice + 2 orders");
    }

    let cust_count = row_count(connection, "mt_customers")?;
    let order_count = row_count(connection, "mt_orders")?;
    println!("  After commit: {cust_count} customer(s), {order_count} order(s)");

    // --- Rolled-back transaction: insert Bob + orders, then rollback ---
    // Both tables must revert atomically.
    {
        let txn = connection.transaction()?;
        txn.execute_command("INSERT INTO mt_customers VALUES (2, 'Bob', 'bob@example.com')")?;
        txn.execute_command("INSERT INTO mt_orders VALUES (201, 2, 'Gadget X', 199.99)")?;
        txn.execute_command("INSERT INTO mt_orders VALUES (202, 2, 'Gadget Y', 149.99)")?;

        // Verify both tables see the uncommitted data inside the transaction
        let c: i64 = txn.query_count("SELECT COUNT(*) FROM mt_customers")?;
        let o: i64 = txn.query_count("SELECT COUNT(*) FROM mt_orders")?;
        println!("  Inside rolled-back txn: {c} customer(s), {o} order(s)");

        txn.rollback()?;
        println!("  Rolled back Bob + his orders");
    }

    // Verify BOTH tables reverted — no orphan orders for Bob
    let cust_count = row_count(connection, "mt_customers")?;
    let order_count = row_count(connection, "mt_orders")?;
    println!("  After rollback: {cust_count} customer(s), {order_count} order(s)");
    println!("  Referential integrity preserved — no orphan orders");

    // Show final state
    println!("  Customers:");
    for row in
        &connection.fetch_all("SELECT customer_id, name FROM mt_customers ORDER BY customer_id")?
    {
        let id: i32 = row.get(0).unwrap();
        let name: String = row.get(1).unwrap();
        println!("    customer_id={id}, name={name}");
    }
    println!("  Orders:");
    for row in &connection.fetch_all(
        "SELECT order_id, customer_id, product, amount FROM mt_orders ORDER BY order_id",
    )? {
        let oid: i32 = row.get(0).unwrap();
        let cid: i32 = row.get(1).unwrap();
        let product: String = row.get(2).unwrap();
        let amount: f64 = row.get(3).unwrap();
        println!("    order_id={oid}, customer_id={cid}, product={product}, amount={amount:.2}");
    }
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Example 6: Multi-table reconnect semantics
//
// Verifies that after a clean connection close/reopen cycle, only committed
// cross-table data is visible — uncommitted state is dropped along with the
// connection. This demonstrates the atomicity/isolation guarantees across
// reconnects on the same `HyperProcess`.
// ---------------------------------------------------------------------------
fn example_multi_table_reconnect(hyper: &HyperProcess) -> Result<()> {
    println!("=== Example 6: Multi-Table Reconnect Semantics ===");

    // Use a fresh database for this example
    let dur_path = "test_results/txn_reconnect.hyper";
    let conn = Connection::new(hyper, dur_path, CreateMode::CreateAndReplace)?;

    let catalog = Catalog::new(&conn);
    let accounts_def = TableDefinition::from("dur_accounts")
        .add_required_column("id", SqlType::int())
        .add_required_column("name", SqlType::text())
        .add_required_column("balance", SqlType::double());
    catalog.create_table(&accounts_def)?;

    let transfers_def = TableDefinition::from("dur_transfers")
        .add_required_column("transfer_id", SqlType::int())
        .add_required_column("from_id", SqlType::int())
        .add_required_column("to_id", SqlType::int())
        .add_required_column("amount", SqlType::double());
    catalog.create_table(&transfers_def)?;
    println!("  Created dur_accounts and dur_transfers tables");

    // --- Committed transaction: create accounts and a transfer ---
    conn.begin_transaction_unguarded()?;
    conn.execute_command("INSERT INTO dur_accounts VALUES (1, 'Alice', 800.0)")?;
    conn.execute_command("INSERT INTO dur_accounts VALUES (2, 'Bob',   700.0)")?;
    conn.execute_command("INSERT INTO dur_transfers VALUES (1, 1, 2, 200.0)")?;
    conn.commit_unguarded()?;
    println!("  Committed: 2 accounts + 1 transfer");

    // --- Uncommitted transaction: another transfer that we don't commit ---
    conn.begin_transaction_unguarded()?;
    conn.execute_command("UPDATE dur_accounts SET balance = balance - 500 WHERE id = 1")?;
    conn.execute_command("UPDATE dur_accounts SET balance = balance + 500 WHERE id = 2")?;
    conn.execute_command("INSERT INTO dur_transfers VALUES (2, 1, 2, 500.0)")?;
    println!("  Started uncommitted transfer of $500 (not committed)");
    // Do NOT commit — drop the connection

    drop(conn);
    println!("  Connection dropped (uncommitted data should be lost)");

    // --- Reconnect and verify ---
    let conn2 = Connection::new(hyper, dur_path, CreateMode::DoNotCreate)?;

    let acct_count: i64 = conn2.query_count("SELECT COUNT(*) FROM dur_accounts")?;
    let xfer_count: i64 = conn2.query_count("SELECT COUNT(*) FROM dur_transfers")?;
    println!("  After reconnect: {acct_count} account(s), {xfer_count} transfer(s)");

    // Verify balances — should reflect only the committed state
    println!("  Accounts (should reflect only committed state):");
    for row in &conn2.fetch_all("SELECT id, name, balance FROM dur_accounts ORDER BY id")? {
        let id: i32 = row.get(0).unwrap();
        let name: String = row.get(1).unwrap();
        let balance: f64 = row.get(2).unwrap();
        println!("    id={id}, name={name}, balance={balance:.2}");
    }

    // Verify only 1 transfer (the committed one)
    println!("  Transfers (should have only the committed transfer):");
    for row in &conn2.fetch_all(
        "SELECT transfer_id, from_id, to_id, amount FROM dur_transfers ORDER BY transfer_id",
    )? {
        let tid: i32 = row.get(0).unwrap();
        let from: i32 = row.get(1).unwrap();
        let to: i32 = row.get(2).unwrap();
        let amount: f64 = row.get(3).unwrap();
        println!("    transfer_id={tid}, from={from}, to={to}, amount={amount:.2}");
    }

    // Cross-table consistency check: sum of balances should be 1500
    // (800 + 700 from original inserts, the uncommitted $500 transfer was lost)
    let total: f64 = conn2.fetch_scalar("SELECT SUM(balance) FROM dur_accounts")?;
    println!("  Total balance: {total:.2} (expected 1500.00 — uncommitted transfer lost)");

    // Verify referential consistency: every transfer references valid accounts
    let bad_refs: i64 = conn2.query_count(
        "SELECT COUNT(*) FROM dur_transfers t \
         WHERE NOT EXISTS (SELECT 1 FROM dur_accounts a WHERE a.id = t.from_id) \
            OR NOT EXISTS (SELECT 1 FROM dur_accounts a WHERE a.id = t.to_id)",
    )?;
    println!("  Dangling transfer references: {bad_refs} (expected 0)");

    drop(conn2);
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Example 7: Auto-rollback on drop (safety net)
//
// If a Transaction guard is dropped without calling commit() or rollback(),
// the Drop implementation issues a best-effort ROLLBACK. This ensures
// uncommitted changes don't accidentally persist.
// ---------------------------------------------------------------------------
fn example_auto_rollback_on_drop(connection: &mut Connection) -> Result<()> {
    println!("=== Example 7: Auto-Rollback on Drop ===");

    create_accounts_table(connection, "auto_rb")?;

    // Committed row — will survive
    connection.execute_command("INSERT INTO auto_rb VALUES (1, 'Alice', 1000.0)")?;
    println!("  Inserted Alice outside any transaction");

    // This block creates a transaction, inserts data, but never commits.
    // When `txn` goes out of scope, Drop triggers ROLLBACK automatically.
    {
        let txn = connection.transaction()?;
        txn.execute_command("INSERT INTO auto_rb VALUES (2, 'Bob', 500.0)")?;
        txn.execute_command("INSERT INTO auto_rb VALUES (3, 'Charlie', 750.0)")?;
        println!("  Inserted Bob and Charlie inside transaction (no commit)");
        // txn is dropped here — auto-rollback kicks in
    }
    println!("  Transaction guard dropped without commit → auto-rollback");

    let count = row_count(connection, "auto_rb")?;
    println!("  Row count: {count} (only Alice — Bob & Charlie were rolled back)");
    print_table(connection, "auto_rb")?;
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Example 8: DDL in transactions and known restrictions
//
// Hyper supports DDL (CREATE TABLE, etc.) inside transactions, but mixing
// DDL with DML (INSERT, UPDATE, DELETE) in the same transaction produces
// error SQLSTATE 0A000. This example shows what works and what doesn't.
// ---------------------------------------------------------------------------
fn example_ddl_in_transactions(connection: &mut Connection) -> Result<()> {
    println!("=== Example 8: DDL in Transactions ===");

    // --- Part A: DDL-only transaction works fine ---
    println!("  Part A: DDL-only transaction (works)");
    {
        let txn = connection.transaction()?;
        txn.execute_command("CREATE TABLE ddl_test (id INT NOT NULL, value TEXT NOT NULL)")?;
        println!("  Created table 'ddl_test' inside transaction");
        txn.commit()?;
        println!("  Committed — table exists");
    }

    // Verify the table is usable
    connection.execute_command("INSERT INTO ddl_test VALUES (1, 'hello')")?;
    let count = row_count(connection, "ddl_test")?;
    println!("  Inserted a row after commit: {count} row(s)");

    // --- Part B: DDL after DML in the same transaction fails ---
    println!();
    println!("  Part B: DDL after DML (restricted)");
    connection.begin_transaction_unguarded()?;
    connection.execute_command("INSERT INTO ddl_test VALUES (2, 'world')")?;
    println!("  Executed DML (INSERT) inside transaction");

    // Attempting DDL after DML will fail with SQLSTATE 0A000
    let ddl_result = connection.execute_command("CREATE TABLE should_fail (x INT)");
    match ddl_result {
        Ok(_) => println!("  DDL after DML succeeded (unexpected)"),
        Err(e) => {
            println!("  DDL after DML error: {e}");
            if let Some(code) = e.sqlstate() {
                println!("  SQLSTATE: {code}");
            }
        }
    }

    // After an error inside a transaction, the transaction is aborted.
    // We must ROLLBACK before the connection can be used again.
    connection.rollback_unguarded()?;
    println!("  Rolled back after error — connection is healthy again");
    println!();
    Ok(())
}
