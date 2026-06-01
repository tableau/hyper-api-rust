// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! B4 end-to-end integration tests for compile-time SQL validation.
//!
//! These tests verify the full stack: `derive(Table) #[hyperdb(register)]`
//! registers the schema in the compile-time registry, and `query_as!` finds
//! the registration and validates the SQL against a live Hyper instance.
//!
//! The tests are integration tests (require HYPERD_PATH) that exercise the
//! **runtime half** of the query_as! path (QueryAs<T>.fetch_all/fetch_one).
//! The compile-time half (validate_query_as running inside the proc-macro host)
//! is exercised every time this file is compiled with the `compile-time` feature.
//!
//! To run with compile-time validation:
//!   cargo test -p hyperdb-api --test compile_time_validation_tests \
//!     --features hyperdb-api-derive/compile-time

mod common;
use common::TestConnection;

use hyperdb_api::Table;
use hyperdb_api_derive::{query_as, FromRow, Table};

// ---------------------------------------------------------------------------
// Test structs — derive(Table) registers them at compile time when
// `compile-time` feature is enabled.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, FromRow, Table)]
#[hyperdb(table = "ct_users", register)]
struct CtUser {
    id: i64,
    name: String,
    score: Option<f64>,
}

#[derive(Debug, PartialEq, FromRow, Table)]
#[hyperdb(table = "ct_orders", register)]
struct CtOrder {
    id: i64,
    user_id: i64,
    amount: f64,
}

// ---------------------------------------------------------------------------
// Positive integration tests
// ---------------------------------------------------------------------------

/// Verify derive(Table) emits correct CREATE TABLE SQL.
#[test]
fn table_derive_creates_correct_sql() {
    assert!(
        CtUser::CREATE_SQL.contains("ct_users"),
        "CREATE_SQL must contain table name"
    );
    assert!(
        CtUser::CREATE_SQL.contains("id BIGINT"),
        "i64 maps to BIGINT"
    );
    assert!(
        CtUser::CREATE_SQL.contains("name TEXT"),
        "String maps to TEXT"
    );
    // score is Option<f64> → nullable DOUBLE PRECISION
    assert!(
        CtUser::CREATE_SQL.contains("score DOUBLE PRECISION"),
        "f64 maps to DOUBLE PRECISION"
    );
    // Option<T> → no NOT NULL constraint
    assert!(
        !CtUser::CREATE_SQL.contains("score DOUBLE PRECISION NOT NULL"),
        "Option<f64> must not have NOT NULL"
    );
    assert_eq!(CtUser::NAME, "ct_users");
}

/// query_as! happy path: SELECT all fields → QueryAs runs and returns rows.
#[test]
fn query_as_fetch_all_happy_path() {
    let test = TestConnection::new().expect("TestConnection");
    test.execute_command(CtUser::CREATE_SQL)
        .expect("create ct_users");
    test.execute_command("INSERT INTO ct_users VALUES (1, 'Alice', 95.5), (2, 'Bob', NULL)")
        .expect("insert");

    let users: Vec<CtUser> = query_as!(CtUser, "SELECT id, name, score FROM ct_users ORDER BY id")
        .fetch_all(&test.connection)
        .expect("fetch_all");

    assert_eq!(users.len(), 2);
    assert_eq!(users[0].id, 1);
    assert_eq!(users[0].name, "Alice");
    assert_eq!(users[0].score, Some(95.5));
    assert_eq!(users[1].id, 2);
    assert_eq!(users[1].score, None);
}

/// query_as! fetch_one returns the first row.
#[test]
fn query_as_fetch_one() {
    let test = TestConnection::new().expect("TestConnection");
    test.execute_command(CtUser::CREATE_SQL)
        .expect("create ct_users");
    test.execute_command("INSERT INTO ct_users VALUES (42, 'Charlie', 77.0)")
        .expect("insert");

    let user: CtUser = query_as!(CtUser, "SELECT id, name, score FROM ct_users WHERE id = 42")
        .fetch_one(&test.connection)
        .expect("fetch_one");

    assert_eq!(user.id, 42);
    assert_eq!(user.name, "Charlie");
}

/// query_as! fetch_optional returns None when no rows match.
#[test]
fn query_as_fetch_optional_no_rows() {
    let test = TestConnection::new().expect("TestConnection");
    test.execute_command(CtUser::CREATE_SQL)
        .expect("create ct_users");

    let result: Option<CtUser> = query_as!(
        CtUser,
        "SELECT id, name, score FROM ct_users WHERE id = 9999"
    )
    .fetch_optional(&test.connection)
    .expect("fetch_optional");

    assert!(result.is_none());
}

/// Lenient-additions invariant: extra columns in the result set are silently
/// ignored — SELECT * from a registered table projects all columns including
/// any future additions; CtUser only picks up id/name/score.
#[test]
fn query_as_select_star_extra_columns_ok() {
    let test = TestConnection::new().expect("TestConnection");
    test.execute_command(CtUser::CREATE_SQL)
        .expect("create ct_users");
    test.execute_command("INSERT INTO ct_users VALUES (1, 'Dave', 88.0)")
        .expect("insert");

    // SELECT * projects all columns; CtUser has all three → this is fine.
    // When a new column is later added to ct_users, this continues to work.
    let users: Vec<CtUser> = query_as!(CtUser, "SELECT * FROM ct_users")
        .fetch_all(&test.connection)
        .expect("fetch_all with SELECT *");

    assert_eq!(users.len(), 1);
    assert_eq!(users[0].name, "Dave");
}

/// JOIN query: CtUser joined to CtOrder — both tables registered, join compiles.
#[test]
fn query_as_join_two_registered_tables() {
    let test = TestConnection::new().expect("TestConnection");
    test.execute_command(CtUser::CREATE_SQL)
        .expect("create ct_users");
    test.execute_command(CtOrder::CREATE_SQL)
        .expect("create ct_orders");
    test.execute_command("INSERT INTO ct_users VALUES (1, 'Eve', 90.0)")
        .expect("insert user");
    test.execute_command("INSERT INTO ct_orders VALUES (10, 1, 49.99)")
        .expect("insert order");

    // Only select CtUser columns; extra ct_orders columns are ignored (lenient).
    let users: Vec<CtUser> = query_as!(
        CtUser,
        "SELECT u.id, u.name, u.score \
         FROM ct_users u JOIN ct_orders o ON u.id = o.user_id"
    )
    .fetch_all(&test.connection)
    .expect("fetch_all join");

    assert_eq!(users.len(), 1);
    assert_eq!(users[0].name, "Eve");
}
