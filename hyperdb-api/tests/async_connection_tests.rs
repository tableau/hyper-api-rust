// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for the full [`AsyncConnection`] API surface.
//!
//! These mirror the sync-side tests in [`connection_tests.rs`] and are
//! the regression harness for async-parity work.

mod common;

use common::{test_hyper_params, test_result_path};
use hyperdb_api::{AsyncConnection, CreateMode, FromRow, HyperProcess, Result};

async fn fresh_async_conn(name: &str) -> Result<(HyperProcess, AsyncConnection)> {
    let db_path = test_result_path(name, "hyper")?;
    let params = test_hyper_params(name)?;
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper.require_endpoint()?.to_string();
    let conn = AsyncConnection::connect(
        &endpoint,
        db_path.to_str().expect("path"),
        CreateMode::CreateAndReplace,
    )
    .await?;
    Ok((hyper, conn))
}

#[tokio::test(flavor = "current_thread")]
async fn execute_query_streaming_chunks() {
    let (_hyper, conn) = fresh_async_conn("async_exec_query_chunks").await.unwrap();

    conn.execute_command("CREATE TABLE t (v INT NOT NULL)")
        .await
        .unwrap();
    for i in 1..=8 {
        conn.execute_command(&format!("INSERT INTO t VALUES ({i})"))
            .await
            .unwrap();
    }

    let mut rs = conn
        .execute_query("SELECT v FROM t ORDER BY v")
        .await
        .unwrap();
    let mut total = 0;
    while let Some(chunk) = rs.next_chunk().await.unwrap() {
        total += chunk.len();
    }
    assert_eq!(total, 8);
    drop(rs);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_family_roundtrip() {
    let (_hyper, conn) = fresh_async_conn("async_fetch_family").await.unwrap();

    conn.execute_command("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .await
        .unwrap();
    conn.execute_command("INSERT INTO t VALUES (1, 'alice'), (2, 'bob'), (3, NULL)")
        .await
        .unwrap();

    // fetch_one
    let row = conn
        .fetch_one("SELECT id, name FROM t WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(row.get::<i32>(0), Some(1));
    assert_eq!(row.get::<String>(1), Some("alice".to_string()));

    // fetch_optional (hit)
    let row = conn
        .fetch_optional("SELECT id FROM t WHERE id = 2")
        .await
        .unwrap();
    assert!(row.is_some());

    // fetch_optional (miss)
    let row = conn
        .fetch_optional("SELECT id FROM t WHERE id = 999")
        .await
        .unwrap();
    assert!(row.is_none());

    // fetch_all
    let rows = conn
        .fetch_all("SELECT id FROM t ORDER BY id")
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);

    // fetch_scalar
    let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count, 3);

    // fetch_optional_scalar (hit)
    let name: Option<String> = conn
        .fetch_optional_scalar("SELECT name FROM t WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(name, Some("alice".to_string()));

    // query_count
    let n = conn
        .query_count("SELECT COUNT(*) FROM t WHERE name IS NOT NULL")
        .await
        .unwrap();
    assert_eq!(n, 2);

    conn.close().await.unwrap();
}

#[derive(Debug, PartialEq)]
struct User {
    id: i32,
    name: Option<String>,
}

impl FromRow for User {
    fn from_row(row: hyperdb_api::RowAccessor<'_>) -> Result<Self> {
        Ok(User {
            id: row.get("id")?,
            name: row.get_opt("name")?,
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_as_struct_mapping() {
    let (_hyper, conn) = fresh_async_conn("async_fetch_as").await.unwrap();

    conn.execute_command("CREATE TABLE users (id INT NOT NULL, name TEXT)")
        .await
        .unwrap();
    conn.execute_command("INSERT INTO users VALUES (1, 'alice'), (2, 'bob')")
        .await
        .unwrap();

    let user: User = conn
        .fetch_one_as("SELECT id, name FROM users WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(
        user,
        User {
            id: 1,
            name: Some("alice".to_string())
        }
    );

    let users: Vec<User> = conn
        .fetch_all_as("SELECT id, name FROM users ORDER BY id")
        .await
        .unwrap();
    assert_eq!(users.len(), 2);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn query_and_command_params() {
    let (_hyper, conn) = fresh_async_conn("async_params").await.unwrap();

    conn.execute_command("CREATE TABLE orders (id INT NOT NULL, total DOUBLE PRECISION)")
        .await
        .unwrap();

    // command_params (INSERT)
    let n = conn
        .command_params("INSERT INTO orders VALUES ($1, $2)", &[&1i32, &99.5_f64])
        .await
        .unwrap();
    assert_eq!(n, 1);

    // command_params (INSERT another)
    conn.command_params("INSERT INTO orders VALUES ($1, $2)", &[&2i32, &200.0_f64])
        .await
        .unwrap();

    // query_params (SELECT with WHERE)
    let rs = conn
        .query_params(
            "SELECT id FROM orders WHERE total > $1 ORDER BY id",
            &[&100.0_f64],
        )
        .await
        .unwrap();
    let rows = rs.collect_rows().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<i32>(0), Some(2));

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn has_table_and_schema() {
    let (_hyper, conn) = fresh_async_conn("async_catalog").await.unwrap();

    assert!(!conn.has_table("nope").await.unwrap());
    conn.execute_command("CREATE TABLE kept (id INT)")
        .await
        .unwrap();
    assert!(conn.has_table("kept").await.unwrap());

    assert!(conn.has_schema("public").await.unwrap());
    assert!(!conn.has_schema("nonexistent_schema").await.unwrap());

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn ping_and_version() {
    let (_hyper, conn) = fresh_async_conn("async_ping").await.unwrap();

    conn.ping().await.unwrap();
    assert!(conn.is_alive());
    // server_version is best-effort — hyperd sets it but older builds
    // may omit; just make sure the getter works.
    let _ = conn.server_version().await;

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn execute_batch_runs_all_statements() {
    let (_hyper, conn) = fresh_async_conn("async_batch").await.unwrap();

    let total = conn
        .execute_batch(&[
            "CREATE TABLE b (id INT NOT NULL)",
            "INSERT INTO b VALUES (1)",
            "INSERT INTO b VALUES (2)",
        ])
        .await
        .unwrap();
    assert!(total >= 2);

    let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM b").await.unwrap();
    assert_eq!(count, 2);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_with_commit() {
    let (_hyper, mut conn) = fresh_async_conn("async_tx_commit").await.unwrap();

    conn.execute_command("CREATE TABLE t (v INT NOT NULL)")
        .await
        .unwrap();
    {
        let txn = conn.transaction().await.unwrap();
        txn.execute_command("INSERT INTO t VALUES (1)")
            .await
            .unwrap();
        txn.execute_command("INSERT INTO t VALUES (2)")
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
    let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count, 2);

    conn.close().await.unwrap();
}
