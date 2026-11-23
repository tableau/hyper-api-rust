// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Typed prepared-statement parameters must round-trip through a real `hyperd`.
//!
//! Regression for `SqlParam` encoding numerics little-endian while
//! `execute_prepared` binds every parameter as PostgreSQL binary (format
//! code 1, big-endian).

mod common;
use common::TestServer;

use hyperdb_api_core::client::{Client, Row, SqlParam};
use hyperdb_api_core::types::{Oid, oids};

/// Prepares `sql` with one explicit parameter type, executes it with `param`,
/// and returns the single result row. Any prepare/execute error panics.
fn run_typed(client: &Client, sql: &str, oid: Oid, param: Vec<u8>) -> Row {
    let stmt = client
        .prepare_typed(sql, &[oid])
        .unwrap_or_else(|e| panic!("prepare_typed({sql}) failed: {e}"));
    let rows = client
        .execute(&stmt, [Some(param)])
        .unwrap_or_else(|e| panic!("execute({sql}) failed: {e}"));
    assert_eq!(rows.len(), 1, "expected one row for {sql}");
    rows.into_iter().next().expect("one row")
}

#[test]
fn typed_params_round_trip() {
    let server = TestServer::new().expect("Failed to create test server");
    let client = server.connect().expect("Failed to connect");

    // Cast to TEXT server-side so the assertion exercises only how the server
    // *decoded the bound parameter*, independent of result-column decoding.
    let cases: Vec<(&str, Oid, Vec<u8>, &str)> = vec![
        (
            "SELECT ($1::SMALLINT)::TEXT",
            oids::SMALL_INT,
            1234_i16.encode(),
            "1234",
        ),
        ("SELECT ($1::INT)::TEXT", oids::INT, 42_i32.encode(), "42"),
        (
            "SELECT ($1::INT)::TEXT",
            oids::INT,
            (-123_456_i32).encode(),
            "-123456",
        ),
        (
            "SELECT ($1::BIGINT)::TEXT",
            oids::BIG_INT,
            9_000_000_000_i64.encode(),
            "9000000000",
        ),
        (
            "SELECT ($1::REAL)::TEXT",
            oids::FLOAT,
            1.5_f32.encode(),
            "1.5",
        ),
        (
            "SELECT ($1::DOUBLE PRECISION)::TEXT",
            oids::DOUBLE,
            (-2.25_f64).encode(),
            "-2.25",
        ),
        (
            "SELECT ($1::BOOLEAN)::TEXT",
            oids::BOOL,
            true.encode(),
            "true",
        ),
        (
            "SELECT ($1::BOOLEAN)::TEXT",
            oids::BOOL,
            false.encode(),
            "false",
        ),
        (
            "SELECT $1::TEXT",
            oids::TEXT,
            "h\u{e9}llo".encode(),
            "h\u{e9}llo",
        ),
    ];
    for (sql, oid, param, expected) in cases {
        let r = run_typed(&client, sql, oid, param);
        assert_eq!(r.get_string(0).as_deref(), Some(expected), "{sql}");
    }

    client
        .exec("CREATE TABLE typed_params (id INT, name TEXT)")
        .expect("create");
    client
        .exec("INSERT INTO typed_params VALUES (1, 'Alice'), (2, 'Bob')")
        .expect("insert");
    let r = run_typed(
        &client,
        "SELECT name FROM typed_params WHERE id = $1",
        oids::INT,
        2_i32.encode(),
    );
    assert_eq!(r.get_string(0).as_deref(), Some("Bob"));

    client.close().expect("Failed to close");
}
