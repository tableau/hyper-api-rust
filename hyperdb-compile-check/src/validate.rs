// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `validate_query_as` — the single entry point for `query_as!` validation.
//!
//! Algorithm:
//! 1. Look up `target_struct` in the registry; if absent → `StructNotRegistered`.
//! 2. Run the `LIMIT 0` dry-run.
//! 3. On success: compute name-subset diff (struct fields ⊆ result columns).
//! 4. On `42P01` (undefined_table): extract the table name, seed from registry
//!    if known (then retry once), or emit `TablesNotRegistered`.
//! 5. On `42703` (undefined_column) or `42601` (syntax): forward as diagnostics.
//!
//! No `syn`/`quote`/`proc-macro2` types in this module's public API.

use crate::db::get_or_init;
use crate::diagnostic::ValidationError;
use crate::dry_run::dry_run;
use crate::error_extract::{classify, ErrorClass};
use crate::registry::{self, Registry};

/// Validate that `sql` is structurally compatible with `target_struct`.
///
/// # Parameters
/// - `target_struct`: the Rust struct ident as a string (for registry lookup +
///   diagnostics).
/// - `sql`: the raw SQL string literal from `query_as!(T, "...")`.
///
/// # Errors
///
/// Returns a [`ValidationError`] if validation fails — e.g. the struct is not
/// registered, SQL has a syntax error, referenced tables are not registered, or
/// the result schema is missing columns the struct requires.
pub fn validate_query_as(target_struct: &str, sql: &str) -> Result<(), ValidationError> {
    // Step 1: struct must be registered.
    let entry =
        registry::get(target_struct).ok_or_else(|| ValidationError::StructNotRegistered {
            struct_name: target_struct.to_owned(),
        })?;

    let mut db = get_or_init().lock();

    // Step 2+4: dry-run with seed-and-retry on 42P01.
    let schema = match dry_run(&mut db, sql) {
        Ok(s) => s,
        Err(e) => {
            return Err(match classify(&e) {
                ErrorClass::MissingTable(table_name) => {
                    // Seed and retry once.
                    match Registry::seed_if_known(&table_name, &mut db) {
                        Ok(true) => {
                            // Seeded; retry the dry-run.
                            match dry_run(&mut db, sql) {
                                Ok(s) => {
                                    // Proceed to name-subset diff below.
                                    drop(db);
                                    return finish_name_check(target_struct, &entry.fields, &s);
                                }
                                Err(retry_err) => match classify(&retry_err) {
                                    ErrorClass::MissingTable(t) => {
                                        ValidationError::TablesNotRegistered { tables: vec![t] }
                                    }
                                    ErrorClass::MissingColumn(col) => ValidationError::HyperError {
                                        message: format!("unknown column {col:?}"),
                                    },
                                    ErrorClass::SyntaxError(msg) => {
                                        ValidationError::SqlSyntaxError { message: msg }
                                    }
                                    ErrorClass::Other(msg) => {
                                        ValidationError::HyperError { message: msg }
                                    }
                                },
                            }
                        }
                        Ok(false) => ValidationError::TablesNotRegistered {
                            tables: vec![table_name],
                        },
                        Err(seed_err) => ValidationError::HyperError {
                            message: format!("{seed_err}"),
                        },
                    }
                }
                ErrorClass::MissingColumn(col) => ValidationError::HyperError {
                    message: format!("unknown column {col:?}"),
                },
                ErrorClass::SyntaxError(msg) => ValidationError::SqlSyntaxError { message: msg },
                ErrorClass::Other(msg) => ValidationError::HyperError { message: msg },
            });
        }
    };

    drop(db);

    // Step 3: name-subset diff.
    finish_name_check(target_struct, &entry.fields, &schema)
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
            "users",
            "CREATE TABLE IF NOT EXISTS users (id BIGINT, name TEXT, email TEXT)",
            vec!["id".into(), "name".into(), "email".into()],
        );
    }

    #[test]
    fn struct_not_registered_error() {
        // No registration for "Ghost" — must get StructNotRegistered.
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
        validate_query_as("users", "SELECT id, name, email FROM users").unwrap();
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn extra_column_in_result_is_ok() {
        // Lenient-additions: SELECT * projects an extra column not in struct.
        registry::register(
            "slim_users",
            "CREATE TABLE IF NOT EXISTS slim_users (id BIGINT, name TEXT, extra TEXT)",
            vec!["id".into(), "name".into()],
        );
        validate_query_as("slim_users", "SELECT * FROM slim_users").unwrap();
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn missing_column_error() {
        setup_users();
        let err = validate_query_as("users", "SELECT id, name FROM users").unwrap_err();
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
        // Register before querying; the table doesn't pre-exist in DB.
        registry::register(
            "orders",
            "CREATE TABLE IF NOT EXISTS orders (id BIGINT, total DOUBLE PRECISION)",
            vec!["id".into(), "total".into()],
        );
        // First call seeds the table via 42P01 seed-and-retry.
        validate_query_as("orders", "SELECT id, total FROM orders").unwrap();
        // Second call: table already seeded, should pass straight through.
        validate_query_as("orders", "SELECT id, total FROM orders").unwrap();
    }

    #[test]
    #[ignore = "requires HYPERD_PATH; run manually"]
    fn unregistered_table_in_sql_error() {
        registry::register(
            "known",
            "CREATE TABLE IF NOT EXISTS known (id BIGINT)",
            vec!["id".into()],
        );
        let err = validate_query_as("known", "SELECT * FROM nonexistent_xyz").unwrap_err();
        assert!(
            matches!(err, ValidationError::TablesNotRegistered { .. }),
            "expected TablesNotRegistered, got: {err}"
        );
    }
}
