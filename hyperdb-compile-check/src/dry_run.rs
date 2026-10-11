// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `LIMIT 0` dry-run helper.
//!
//! Wraps arbitrary user SQL in a CTE and runs it against the shared
//! `CompileTimeDb`, returning the `ResultSchema` without touching any rows.
//!
//! # Query execution is lazy
//!
//! `Connection::execute_query()` does NOT run the query on the TCP transport.
//! The query only executes — and server errors / the `RowDescription` (schema)
//! only arrive — when `Rowset::next_chunk()` first pulls bytes. Therefore:
//! - `execute_query(sql).is_err()` alone always looks `Ok`.
//! - This helper calls `next_chunk()` once to force execution, then reads
//!   `Rowset::schema()`.

use hyperdb_api::{Error, Result, ResultSchema};

use crate::db::CompileTimeDb;

/// Wrap `user_sql` in a `LIMIT 0` CTE and return the projected `ResultSchema`.
///
/// Uses the `__hdb_q` CTE prefix to minimize collision with user-supplied CTE
/// names.
///
/// # Errors
///
/// Returns the Hyper error on any failure (callers branch on SQLSTATE via
/// [`crate::error_extract::classify`]).
pub fn dry_run(db: &mut CompileTimeDb, user_sql: &str) -> Result<ResultSchema> {
    let wrapped = format!("WITH __hdb_q AS ({user_sql}) SELECT * FROM __hdb_q LIMIT 0");
    // A `$N` placeholder can't be executed without bound values, and Hyper
    // rejects NULL in some positions. Preparing the statement (Parse/Describe,
    // no Bind) lets the server infer the parameter types and still report the
    // result schema, which is all validation needs.
    let placeholders = placeholder_count(user_sql);
    if placeholders > 0 {
        let oids = vec![hyperdb_api::Oid::new(0); placeholders];
        return Ok(db.conn.prepare_typed(&wrapped, &oids)?.schema().clone());
    }
    let mut rowset = db.conn.execute_query(&wrapped)?;

    // Force execution: LIMIT 0 returns Ok(None) from next_chunk
    // but populates the schema cache first.
    rowset.next_chunk()?;

    rowset
        .schema()
        .ok_or_else(|| Error::Protocol("dry-run: schema missing after next_chunk".into()))
}

/// The highest `$N` placeholder in `sql`, ignoring quoted strings and
/// identifiers. Returns 0 when there are none.
fn placeholder_count(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut max = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
            }
            b'$' => {
                let digits = bytes[i + 1..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                if digits > 0
                    && let Ok(n) = sql[i + 1..i + 1 + digits].parse::<usize>()
                {
                    max = max.max(n);
                }
                i += digits;
            }
            _ => {}
        }
        i += 1;
    }
    max
}

#[cfg(test)]
mod placeholder_tests {
    use super::placeholder_count;

    #[test]
    fn counts_highest_placeholder() {
        assert_eq!(placeholder_count("SELECT 1"), 0);
        assert_eq!(placeholder_count("SELECT * FROM t WHERE a = $1"), 1);
        assert_eq!(placeholder_count("WHERE a = $2 AND b = $1"), 2);
        assert_eq!(placeholder_count("WHERE a = $10"), 10);
    }

    #[test]
    fn ignores_quoted_text() {
        assert_eq!(placeholder_count("SELECT '$3', \"$4\""), 0);
        assert_eq!(placeholder_count("SELECT 'it''s $5' WHERE a = $1"), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::get_or_init;

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn dry_run_plain_select() {
        let mut db = get_or_init().lock();
        db.conn
            .execute_command("CREATE TABLE IF NOT EXISTS _dr_test (id BIGINT, name TEXT)")
            .unwrap();
        let schema = dry_run(&mut db, "SELECT id, name FROM _dr_test").unwrap();
        let names: Vec<_> = schema
            .columns()
            .iter()
            .map(hyperdb_api::ResultColumn::name)
            .collect();
        assert_eq!(names, &["id", "name"]);
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn dry_run_cte_wrapper() {
        let mut db = get_or_init().lock();
        db.conn
            .execute_command("CREATE TABLE IF NOT EXISTS _dr_cte (x INT, y TEXT)")
            .unwrap();
        let schema = dry_run(
            &mut db,
            "WITH src AS (SELECT x, y FROM _dr_cte) SELECT * FROM src",
        )
        .unwrap();
        assert_eq!(schema.column_count(), 2);
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn dry_run_from_less_expression() {
        let mut db = get_or_init().lock();
        let schema = dry_run(&mut db, "SELECT 1 AS a, 'x' AS b").unwrap();
        let names: Vec<_> = schema
            .columns()
            .iter()
            .map(hyperdb_api::ResultColumn::name)
            .collect();
        assert_eq!(names, &["a", "b"]);
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn dry_run_with_bind_placeholders() {
        let mut db = get_or_init().lock();
        db.conn
            .execute_command("CREATE TEMP TABLE IF NOT EXISTS _dr_bind (id BIGINT, name TEXT)")
            .unwrap();
        let schema = dry_run(
            &mut db,
            "SELECT id, name FROM _dr_bind WHERE name = $1 AND id >= $2",
        )
        .unwrap();
        assert_eq!(schema.column_count(), 2);
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn dry_run_bad_table_returns_error() {
        let mut db = get_or_init().lock();
        let err = dry_run(&mut db, "SELECT * FROM _nonexistent_xyz").unwrap_err();
        assert_eq!(
            err.sqlstate(),
            Some("42P01"),
            "expected undefined_table: {err}"
        );
    }
}
