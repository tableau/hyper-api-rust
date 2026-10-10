// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the read-only SQL classifier and the read-only server flag.

use hyperdb_mcp::engine::{StatementKind, classify_statement, is_read_only_sql};
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
    // Block comments do not nest in Hyper: the comment ends at the first
    // `*/`, so `still outer */ SELECT 1` is live (and invalid) SQL.
    assert!(!is_read_only_sql(
        "/* outer /* inner */ still outer */ SELECT 1"
    ));
    // Mixed
    assert!(is_read_only_sql("-- line\n/* block */ SELECT 1"));
    // Comments before mutating statement — must still be rejected
    assert!(!is_read_only_sql("/* bypass */ DROP TABLE t"));
    assert!(!is_read_only_sql("-- sneaky\nDELETE FROM t"));
    assert!(!is_read_only_sql("-- sneaky\rDROP TABLE t"));
}

/// `WITH` takes the kind of its main statement: Hyper runs
/// `WITH ... DELETE` / `INSERT` (measured against hyperd 0.0.26359).
#[test]
fn with_takes_the_kind_of_its_main_statement() {
    for sql in [
        "WITH x AS (SELECT 1) DELETE FROM t",
        "with x as (select 1) insert into t select * from x",
        "WITH x AS (SELECT 1) UPDATE t SET a = 1",
        "WITH x AS (SELECT 1) MERGE INTO t USING x ON true WHEN MATCHED THEN DELETE",
        "WITH RECURSIVE x(n) AS (SELECT 1) DELETE FROM t",
        // `SELECT ... INTO` creates a table (Hyper refuses it by default).
        "SELECT * INTO t2 FROM t",
        "WITH x AS (SELECT 1) SELECT * INTO t2 FROM x",
        "(SELECT 1 INTO t2)",
        "SELECT 1 UNION SELECT 2 INTO t2",
        // A writable CTE body (Hyper rejects these today).
        "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
        "WITH x AS (SELECT 1) (DELETE FROM t)",
    ] {
        assert!(!is_read_only_sql(sql), "{sql}");
    }
    for sql in [
        "WITH x AS (SELECT 1), y AS (SELECT 2) SELECT * FROM x, y",
        "WITH x AS (SELECT 1) (SELECT * FROM x)",
        "WITH x AS (SELECT 1) (SELECT * FROM x) ORDER BY 1",
        "WITH x AS (SELECT 1) VALUES (1)",
        "WITH x AS (SELECT 1) TABLE x",
        // A depth-0 `INSERT` after the main `SELECT` is not the main statement.
        "WITH x AS (SELECT 1) SELECT 'INSERT', \"delete\" FROM x",
        // A quoted `"into"` is an identifier, not the keyword.
        "SELECT * FROM (SELECT 1) AS \"into\"",
    ] {
        assert!(is_read_only_sql(sql), "{sql}");
    }
}

/// `EXPLAIN (ANALYZE) ...` runs its statement in Hyper; a plain `EXPLAIN`
/// only plans.
#[test]
fn explain_with_options_takes_the_kind_of_its_statement() {
    for sql in [
        "EXPLAIN (ANALYZE) DELETE FROM t",
        "explain (analyze) with x as (select 1) delete from t",
        "EXPLAIN (\"analyze\") DELETE FROM t",
        "EXPLAIN (FORMAT JSON, ANALYZE) INSERT INTO t VALUES (1)",
        "EXPLAIN (ANALYZE) TRUNCATE t",
        "EXPLAIN ANALYZE DELETE FROM t",
        "EXPLAIN ANALYZE VERBOSE DELETE FROM t",
        "EXPLAIN",
    ] {
        assert!(!is_read_only_sql(sql), "{sql}");
    }
    for sql in [
        "EXPLAIN SELECT 1",
        "EXPLAIN DELETE FROM t",
        "EXPLAIN (ANALYZE) SELECT 1",
        "EXPLAIN (ANALYZE) WITH x AS (SELECT 1) SELECT * FROM x",
    ] {
        assert!(is_read_only_sql(sql), "{sql}");
    }
}

/// Literals and quoted identifiers hide their contents, tokenized the way
/// Hyper's lexer does.
#[test]
fn literals_hide_their_contents() {
    for sql in [
        "SELECT ')' AS a",
        "SELECT 'a;b' AS a",
        "SELECT 'DELETE' AS a",
        "SELECT 'it''s' AS a",
        "SELECT 'a\\' AS a",
        "SELECT E'a\\'b)' AS a",
        "SELECT $$a)b; DELETE$$ AS a",
        "SELECT $q$a)b$q$ AS a",
        "SELECT \"a)\"\"b\" FROM t",
        "SELECT 'a'\n')b' AS a",
        "SELECT 'a' -- c\n')b' AS a",
        "TABLE t",
        "(SELECT 1)",
        "(SELECT 1) UNION (SELECT 2)",
        "SHOW search_path",
        "SELECT 1;",
        "SELECT 1; -- done",
        "SELECT $1",
        "SELECT\u{a0}1",
        "\u{feff}SELECT 1",
    ] {
        assert!(is_read_only_sql(sql), "{sql:?}");
    }
    // A plain string keeps `\` literal, so this string ends at the second
    // quote and the `)` that follows is live, unbalancing the statement.
    assert!(!is_read_only_sql("SELECT 'a\\') AS a"));
    // Dollar-quote tags are case-sensitive: `$Q$` does not close `$q$`.
    assert!(!is_read_only_sql("SELECT $q$a$Q$ AS a"));
    // A string continuation stays in escape mode, so `\'` does not end it.
    assert!(is_read_only_sql("SELECT E'a'\n'\\') DELETE' AS a"));
    // Without a newline the second `'...'` is a separate (plain) string,
    // which ends at `\'`, leaving `) DELETE'` unterminated.
    assert!(!is_read_only_sql("SELECT E'a' '\\') DELETE' AS a"));
}

/// A comment Hyper closes early cannot hide a statement.
#[test]
fn non_nesting_comments_cannot_hide_a_statement() {
    assert!(!is_read_only_sql("/* /* */ DELETE FROM t; /* */ SELECT 1"));
    assert!(!is_read_only_sql("/* /* */ DELETE FROM t -- */ SELECT 1"));
    assert!(is_read_only_sql("/* a /* b */ SELECT 1"));
}

/// Input Hyper would not parse as one statement is never read-only.
#[test]
fn malformed_or_multiple_statements_are_other() {
    for sql in [
        "SELECT 'unterminated",
        "SELECT \"unterminated",
        "SELECT $$unterminated",
        "SELECT $tag",
        "SELECT 1 /* unterminated",
        "SELECT 1)",
        "SELECT (1",
        "SELECT 1) TO '/tmp/x' WITH (format => 'csv') --",
        "SELECT 1; DELETE FROM t",
        "SELECT 1;;",
    ] {
        assert_eq!(classify_statement(sql), StatementKind::Other, "{sql}");
    }
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
