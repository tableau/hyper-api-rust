// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for prepared statements in hyper-client.
//!
//! NOTE: Hyper has limited support for prepared statements.
//! These tests verify the basic infrastructure works.

mod common;
use common::TestServer;

// =============================================================================
// Basic Prepared Statement Tests
// =============================================================================

#[test]
fn test_prepare_and_execute_basic() {
    let server = TestServer::new().expect("Failed to create test server");
    let client = server.connect().expect("Failed to connect");

    // Prepare a simple statement
    let stmt = client
        .prepare("SELECT 42 AS value")
        .expect("Failed to prepare");

    // Execute it
    let rows = client
        .execute(&stmt, hyperdb_api_core::params![])
        .expect("Failed to execute");

    // Should return at least one row
    assert!(!rows.is_empty(), "Should return results");

    client.close().expect("Failed to close");
}

#[test]
fn test_prepare_with_table() {
    let server = TestServer::new().expect("Failed to create test server");
    let client = server.connect().expect("Failed to connect");

    client
        .exec("CREATE TABLE test_prep (id INT)")
        .expect("Failed to create table");

    client
        .exec("INSERT INTO test_prep VALUES (1), (2), (3)")
        .expect("Failed to insert");

    // Prepare a statement querying the table
    let stmt = client
        .prepare("SELECT id FROM test_prep ORDER BY id")
        .expect("Failed to prepare");

    let rows = client
        .execute(&stmt, hyperdb_api_core::params![])
        .expect("Failed to execute");

    // Should return 3 rows
    assert_eq!(rows.len(), 3, "Should return 3 rows");

    client.close().expect("Failed to close");
}

#[test]
fn test_prepare_reuse() {
    let server = TestServer::new().expect("Failed to create test server");
    let client = server.connect().expect("Failed to connect");

    // Prepare a statement
    let stmt = client
        .prepare("SELECT 1 AS one")
        .expect("Failed to prepare");

    // Execute multiple times - should all succeed
    for i in 0..5 {
        let rows = client
            .execute(&stmt, hyperdb_api_core::params![])
            .unwrap_or_else(|_| panic!("Failed to execute iteration {i}"));
        assert_eq!(rows.len(), 1, "Iteration {i} should return 1 row");
    }

    client.close().expect("Failed to close");
}

#[test]
fn test_multiple_prepared_statements() {
    let server = TestServer::new().expect("Failed to create test server");
    let client = server.connect().expect("Failed to connect");

    // Prepare multiple statements
    let stmt1 = client
        .prepare("SELECT 10 AS a")
        .expect("Failed to prepare stmt1");

    let stmt2 = client
        .prepare("SELECT 20 AS b")
        .expect("Failed to prepare stmt2");

    // Execute them in different order
    let rows2 = client
        .execute(&stmt2, hyperdb_api_core::params![])
        .expect("Execute stmt2");
    assert_eq!(rows2.len(), 1);

    let rows1 = client
        .execute(&stmt1, hyperdb_api_core::params![])
        .expect("Execute stmt1");
    assert_eq!(rows1.len(), 1);

    // Execute again
    let rows1_again = client
        .execute(&stmt1, hyperdb_api_core::params![])
        .expect("Execute stmt1 again");
    assert_eq!(rows1_again.len(), 1);

    client.close().expect("Failed to close");
}

// =============================================================================
// Error Cases
// =============================================================================

#[test]
fn test_prepare_invalid_sql() {
    let server = TestServer::new().expect("Failed to create test server");
    let client = server.connect().expect("Failed to connect");

    // Invalid SQL should fail to prepare
    let result = client.prepare("SELEKT * FORM nowhere");
    assert!(result.is_err(), "Should fail to prepare invalid SQL");

    // Connection should still be usable
    client
        .exec("SELECT 1")
        .expect("Connection should still work");

    client.close().expect("Failed to close");
}

#[test]
fn test_prepare_nonexistent_table() {
    let server = TestServer::new().expect("Failed to create test server");
    let client = server.connect().expect("Failed to connect");

    // Querying nonexistent table - may fail at prepare or execute time
    let result = client.prepare("SELECT * FROM table_that_does_not_exist");

    if let Ok(stmt) = result {
        // If prepare succeeds, execute should fail
        let exec_result = client.execute(&stmt, hyperdb_api_core::params![]);
        assert!(exec_result.is_err(), "Should fail for nonexistent table");
    } else {
        // Failing at prepare time is also acceptable
    }

    client.close().expect("Failed to close");
}

// =============================================================================
// Lifecycle Tests
// =============================================================================

#[test]
fn test_prepared_statement_dropped() {
    let server = TestServer::new().expect("Failed to create test server");
    let client = server.connect().expect("Failed to connect");

    {
        let stmt = client
            .prepare("SELECT 100 AS val")
            .expect("Failed to prepare");

        let rows = client
            .execute(&stmt, hyperdb_api_core::params![])
            .expect("Execute");
        assert_eq!(rows.len(), 1);
        // stmt is dropped here
    }

    // Should be able to create new statements after dropping
    let stmt2 = client
        .prepare("SELECT 200 AS val")
        .expect("Failed to prepare after drop");

    let rows = client
        .execute(&stmt2, hyperdb_api_core::params![])
        .expect("Execute");
    assert_eq!(rows.len(), 1);

    client.close().expect("Failed to close");
}

// =============================================================================
// Parameterized Queries (Limited Support in Hyper)
// =============================================================================

#[test]
fn test_parameterized_query_support() {
    let server = TestServer::new().expect("Failed to create test server");
    let client = server.connect().expect("Failed to connect");

    client
        .exec("CREATE TABLE test_params (id INT, name TEXT)")
        .expect("Failed to create table");

    client
        .exec("INSERT INTO test_params VALUES (1, 'Alice'), (2, 'Bob')")
        .expect("Failed to insert");

    // Hyper rejects untyped `$n` placeholders in plain `prepare()` (it cannot
    // infer the type), so declare the parameter type explicitly. Every other
    // outcome is a failure: this test must not silently skip.
    let stmt = client
        .prepare_typed(
            "SELECT name FROM test_params WHERE id = $1",
            &[hyperdb_api_core::types::oids::INT],
        )
        .expect("prepare_typed with explicit INT parameter must succeed");
    let rows = client
        .execute(&stmt, hyperdb_api_core::params![2_i32])
        .expect("execute with bound INT parameter must succeed");
    assert_eq!(rows.len(), 1, "id = 2 must match exactly one row");
    assert_eq!(rows[0].get_string(0).as_deref(), Some("Bob"));

    client.close().expect("Failed to close");
}
