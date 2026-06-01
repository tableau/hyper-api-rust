// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compile-time SQL validation example.
//!
//! Demonstrates the `compile-time` feature of `hyperdb-api-derive`:
//! - `#[derive(Table)]` generates `CREATE TABLE` SQL and registers the schema
//! - `query_as!` validates SQL against registered structs at compile time
//! - `query_scalar!` validates single-column queries
//!
//! Run with:
//!   HYPERD_PATH=... cargo run --example compile_time_validation \
//!     --features hyperdb-api-derive/compile-time
//!
//! Without the feature flag the example still works; validation is skipped.

use hyperdb_api::{Connection, CreateMode, HyperProcess, QueryAs, QueryScalar, Result, Table};
use hyperdb_api_derive::{query_as, query_scalar, FromRow, Table};

// derive(Table) generates:
//   - impl Table for User { const NAME = "users"; const CREATE_SQL = "..."; }
//   - (with compile-time feature + register) registers User in the compile-time
//     registry so query_as!(User, ...) can validate SQL at build time.
#[derive(Debug, FromRow, Table)]
#[hyperdb(table = "users", register)]
#[allow(
    dead_code,
    reason = "example struct; all fields read via Debug + direct access"
)]
struct User {
    #[hyperdb(primary_key)]
    id: i64,
    name: String,
    email: Option<String>,
    score: f64,
}

fn main() -> Result<()> {
    println!("CREATE TABLE SQL:");
    println!("  {}", User::CREATE_SQL);
    println!();

    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, "example_ct.hyper", CreateMode::CreateAndReplace)?;

    // Use the derived CREATE_SQL to create the table — no hardcoded DDL.
    conn.execute_command(User::CREATE_SQL)?;
    conn.execute_command(
        "INSERT INTO users VALUES \
         (1, 'Alice', 'alice@example.com', 95.5), \
         (2, 'Bob', NULL, 87.0), \
         (3, 'Charlie', 'charlie@example.com', 72.3)",
    )?;

    // query_as! — validated at build time if compile-time feature is enabled.
    // At runtime, returns a QueryAs<User> builder.
    let all_users: Vec<User> =
        query_as!(User, "SELECT * FROM users ORDER BY id").fetch_all(&conn)?;

    println!("All users:");
    for u in &all_users {
        println!("  {u:?}");
    }

    // fetch_one — returns exactly one row (errors if zero rows).
    let alice: User = query_as!(User, "SELECT * FROM users WHERE id = 1").fetch_one(&conn)?;
    println!("\nAlice: {alice:?}");

    // fetch_optional — returns None if no rows.
    let ghost: Option<User> =
        query_as!(User, "SELECT * FROM users WHERE id = 9999").fetch_optional(&conn)?;
    println!("Ghost (should be None): {ghost:?}");

    // query_scalar! — single-column queries.
    let count: i64 = query_scalar!(i64, "SELECT COUNT(*) FROM users").fetch_one(&conn)?;
    println!("\nUser count: {count}");

    let names: Vec<String> =
        query_scalar!(String, "SELECT name FROM users ORDER BY name").fetch_all(&conn)?;
    println!("Names: {names:?}");

    // Demonstrate that QueryAs and QueryScalar are plain builder types —
    // you can store and reuse them.
    let q: QueryAs<User> = query_as!(User, "SELECT * FROM users WHERE score > 80.0");
    let high_scorers = q.fetch_all(&conn)?;
    println!("\nHigh scorers (score > 80):");
    for u in &high_scorers {
        println!("  {} ({})", u.name, u.score);
    }

    let max_q: QueryScalar<f64> = query_scalar!(f64, "SELECT MAX(score) FROM users");
    let max_score: Option<f64> = max_q.fetch_optional(&conn)?;
    println!("Max score: {max_score:?}");

    Ok(())
}
