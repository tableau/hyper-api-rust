// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `validate_query_as` — the single entry point for `query_as!` validation.
//!
//! Algorithm:
//! 1. Look up `struct_name` in the registry via `get_by_struct`; if absent →
//!    `StructNotRegistered`. This returns the SQL table name alongside the entry
//!    (struct ident ≠ SQL table name in general).
//! 2. Run the `LIMIT 0` dry-run.
//! 3. On success: compute name-subset diff (struct fields ⊆ result columns).
//! 4. On `42P01` (undefined_table): extract the SQL table name from the error,
//!    seed from registry if known (then retry once), or emit `TablesNotRegistered`.
//! 5. On `42703` (undefined_column) or `42601` (syntax): forward as diagnostics.
//!
//! # Key design note: struct name vs. table name
//!
//! The registry is dual-indexed: by SQL table name (for 42P01 seed-and-retry,
//! which receives the SQL name from Hyper) and by Rust struct ident (for the
//! initial lookup, since `query_as!(User, …)` passes "User", not "users").
//! `validate_query_as` always receives the struct ident; `Registry::seed_if_known`
//! always receives the SQL table name from Hyper's error.
//!
//! No `syn`/`quote`/`proc-macro2` types in this module's public API.

use crate::db::get_or_init;
use crate::diagnostic::ValidationError;
use crate::dry_run::dry_run;
use crate::error_extract::{classify, ErrorClass};
use crate::registry::{self, Registry};

/// Validate that `sql` is structurally compatible with `struct_name`.
///
/// # Parameters
/// - `struct_name`: the Rust struct ident as a string (e.g. `"User"`).
///   Used for registry lookup via the struct→table index and for diagnostics.
/// - `sql`: the raw SQL string literal from `query_as!(T, "...")`.
///
/// # Errors
///
/// Returns a [`ValidationError`] if validation fails — e.g. the struct is not
/// registered, SQL has a syntax error, referenced tables are not registered, or
/// the result schema is missing columns the struct requires.
pub fn validate_query_as(struct_name: &str, sql: &str) -> Result<(), ValidationError> {
    // Step 1: look up by struct ident (not SQL table name — they differ).
    let (_table_name, entry) = registry::get_by_struct(struct_name).ok_or_else(|| {
        ValidationError::StructNotRegistered {
            struct_name: struct_name.to_owned(),
        }
    })?;

    let mut db = get_or_init().lock();

    // Step 2+4: dry-run with bounded seed-and-retry on 42P01.
    let schema = run_dry_run_with_seed(sql, &mut db)?;

    drop(db);

    // Step 3: name-subset diff.
    finish_name_check(struct_name, &entry.fields, &schema)
}

/// Validate a scalar SQL string: runs the dry-run and checks the result
/// projects exactly one column. Used by `query_scalar!`.
///
/// Does not require a struct registration — scalars project a single column
/// of a primitive type without mapping to a struct. However, tables referenced
/// by the SQL still need to be registered via `derive(Table) #[hyperdb(register)]`
/// for compile-time validation to work; unregistered tables produce a
/// `TablesNotRegistered` diagnostic.
///
/// # Errors
///
/// Returns a [`ValidationError`] if the SQL is invalid, references an
/// unregistered table, or the result schema does not have exactly one column.
pub fn validate_scalar_sql(sql: &str) -> Result<(), ValidationError> {
    let mut db = get_or_init().lock();

    let schema = run_dry_run_with_seed(sql, &mut db)?;

    drop(db);

    let col_count = schema.column_count();
    if col_count != 1 {
        return Err(ValidationError::HyperError {
            message: format!(
                "query_scalar! requires exactly one projected column, but the query projects {col_count}"
            ),
        });
    }

    Ok(())
}

/// Shared dry-run helper with bounded seed-and-retry on SQLSTATE 42P01.
///
/// Loops up to `MAX_SEED_ROUNDS` times: on each 42P01, extracts the missing
/// table name, seeds it from the registry if known, and retries. This handles
/// multi-table queries (JOINs) where several registered tables need seeding
/// before the first `query_as!` invocation in a crate — without the loop,
/// only the first missing table would be seeded per call.
///
/// Stops early on syntax errors, missing-column errors, or unregistered tables.
fn run_dry_run_with_seed(
    sql: &str,
    db: &mut crate::db::CompileTimeDb,
) -> Result<hyperdb_api::ResultSchema, ValidationError> {
    // Bound to prevent infinite loops on pathological SQL (e.g., a self-join
    // that repeatedly 42P01s on the same unregistered table after seeding).
    const MAX_SEED_ROUNDS: usize = 8;

    for _ in 0..MAX_SEED_ROUNDS {
        match dry_run(db, sql) {
            Ok(schema) => return Ok(schema),
            Err(e) => match classify(&e) {
                ErrorClass::MissingTable(t) => match Registry::seed_if_known(&t, db) {
                    Ok(true) => {} // seeded successfully; loop iterates to retry the dry-run
                    Ok(false) => {
                        return Err(ValidationError::TablesNotRegistered { tables: vec![t] })
                    }
                    Err(seed_err) => {
                        return Err(ValidationError::HyperError {
                            message: format!("{seed_err}"),
                        })
                    }
                },
                ErrorClass::SyntaxError(msg) => {
                    return Err(ValidationError::SqlSyntaxError { message: msg })
                }
                ErrorClass::MissingColumn(col) => {
                    return Err(ValidationError::UnknownColumn { column: col })
                }
                ErrorClass::Other(msg) => return Err(ValidationError::HyperError { message: msg }),
            },
        }
    }

    Err(ValidationError::HyperError {
        message: format!(
            "compile-time validation exceeded {MAX_SEED_ROUNDS} seed-and-retry rounds; \
             ensure all tables referenced by this query are registered via \
             `#[derive(Table)] #[hyperdb(register)]`"
        ),
    })
}

/// Check that every field in `struct_fields` appears as a column name in
/// `schema`. Extra columns in the result are fine (lenient-additions contract).
fn finish_name_check(
    struct_name: &str,
    struct_fields: &[String],
    schema: &hyperdb_api::ResultSchema,
) -> Result<(), ValidationError> {
    let result_cols: std::collections::HashSet<&str> = schema
        .columns()
        .iter()
        .map(hyperdb_api::ResultColumn::name)
        .collect();

    let missing: Vec<String> = struct_fields
        .iter()
        .filter(|f| !result_cols.contains(f.as_str()))
        .cloned()
        .collect();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(ValidationError::MissingColumns {
            struct_name: struct_name.to_owned(),
            missing,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_users() {
        registry::register(
            "User",
            "users",
            "CREATE TABLE IF NOT EXISTS users (id BIGINT, name TEXT, email TEXT)",
            vec!["id".into(), "name".into(), "email".into()],
        );
    }

    #[test]
    fn struct_not_registered_error() {
        let err = validate_query_as("Ghost", "SELECT 1").unwrap_err();
        assert!(
            matches!(err, ValidationError::StructNotRegistered { .. }),
            "expected StructNotRegistered, got: {err}"
        );
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn valid_query_passes() {
        setup_users();
        validate_query_as("User", "SELECT id, name, email FROM users").unwrap();
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn extra_column_in_result_is_ok() {
        registry::register(
            "SlimUser",
            "slim_users",
            "CREATE TABLE IF NOT EXISTS slim_users (id BIGINT, name TEXT, extra TEXT)",
            vec!["id".into(), "name".into()],
        );
        validate_query_as("SlimUser", "SELECT * FROM slim_users").unwrap();
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn missing_column_error() {
        setup_users();
        let err = validate_query_as("User", "SELECT id, name FROM users").unwrap_err();
        assert!(
            matches!(err, ValidationError::MissingColumns { .. }),
            "expected MissingColumns, got: {err}"
        );
        let msg = err.to_diagnostic();
        assert!(
            msg.contains("email"),
            "missing column name in message: {msg}"
        );
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn seed_and_retry_on_missing_table() {
        registry::register(
            "Order",
            "orders",
            "CREATE TABLE IF NOT EXISTS orders (id BIGINT, total DOUBLE PRECISION)",
            vec!["id".into(), "total".into()],
        );
        validate_query_as("Order", "SELECT id, total FROM orders").unwrap();
        validate_query_as("Order", "SELECT id, total FROM orders").unwrap();
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn unregistered_table_in_sql_error() {
        registry::register(
            "Known",
            "known",
            "CREATE TABLE IF NOT EXISTS known (id BIGINT)",
            vec!["id".into()],
        );
        let err = validate_query_as("Known", "SELECT * FROM nonexistent_xyz").unwrap_err();
        assert!(
            matches!(err, ValidationError::TablesNotRegistered { .. }),
            "expected TablesNotRegistered, got: {err}"
        );
    }
}
