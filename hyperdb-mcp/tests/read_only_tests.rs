// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the read-only SQL classifier and the read-only server flag.

use hyperdb_mcp::engine::is_read_only_sql;
use hyperdb_mcp::server::HyperMcpServer;

/// Classifier accepts the canonical read-only statement kinds.
#[test]
fn classifies_read_only_statements() {
    assert!(is_read_only_sql("SELECT * FROM t"));
    assert!(is_read_only_sql("select 1"));
    assert!(is_read_only_sql("  SELECT   foo FROM bar"));
    assert!(is_read_only_sql("WITH cte AS (SELECT 1) SELECT * FROM cte"));
    assert!(is_read_only_sql("with cte as (select 1) select 1"));
    assert!(is_read_only_sql("EXPLAIN SELECT 1"));
    assert!(is_read_only_sql("SHOW TABLES"));
    assert!(is_read_only_sql("VALUES (1, 2)"));
}

/// Classifier rejects mutating statements.
#[test]
fn classifies_mutating_statements() {
    assert!(!is_read_only_sql("CREATE TABLE t (a INT)"));
    assert!(!is_read_only_sql("create table t (a int)"));
    assert!(!is_read_only_sql("INSERT INTO t VALUES (1)"));
    assert!(!is_read_only_sql("UPDATE t SET a = 1"));
    assert!(!is_read_only_sql("DELETE FROM t"));
    assert!(!is_read_only_sql("DROP TABLE t"));
    assert!(!is_read_only_sql("ALTER TABLE t ADD COLUMN b INT"));
    assert!(!is_read_only_sql(
        "COPY t FROM '/tmp/data.csv' WITH (FORMAT csv)"
    ));
    assert!(!is_read_only_sql("TRUNCATE t"));
}

/// Classifier handles edge cases: empty input, whitespace, comments-only.
#[test]
fn classifies_edge_cases() {
    assert!(!is_read_only_sql(""));
    assert!(!is_read_only_sql("   "));
    // Comments-only (no actual SQL)
    assert!(!is_read_only_sql("-- just a comment"));
    assert!(!is_read_only_sql("/* block comment only */"));
}

/// Leading SQL comments are stripped before classification (security fix).
#[test]
fn strips_leading_comments() {
    // Line comments (LF)
    assert!(is_read_only_sql("-- comment\nSELECT 1"));
    assert!(is_read_only_sql("-- line1\n-- line2\nSELECT 1"));
    // Line comments (CRLF and CR)
    assert!(is_read_only_sql("-- comment\r\nSELECT 1"));
    assert!(is_read_only_sql("-- comment\rSELECT 1"));
    // Block comments
    assert!(is_read_only_sql("/* comment */ SELECT 1"));
    assert!(is_read_only_sql("/* multi\nline */ SELECT 1"));
    // Nested block comments
    assert!(is_read_only_sql(
        "/* outer /* inner */ still outer */ SELECT 1"
    ));
    // Mixed
    assert!(is_read_only_sql("-- line\n/* block */ SELECT 1"));
    // Comments before mutating statement — must still be rejected
    assert!(!is_read_only_sql("/* bypass */ DROP TABLE t"));
    assert!(!is_read_only_sql("-- sneaky\nDELETE FROM t"));
    assert!(!is_read_only_sql("-- sneaky\rDROP TABLE t"));
}

/// Verify a read-only server reports itself as read-only and a writable
/// server does not.
#[test]
fn server_read_only_flag_is_respected() {
    let ro = HyperMcpServer::with_no_daemon(None, true, true);
    assert!(ro.is_read_only());

    let rw = HyperMcpServer::with_no_daemon(None, false, true);
    assert!(!rw.is_read_only());
}
