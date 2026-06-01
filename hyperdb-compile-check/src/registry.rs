// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Table and struct registry for compile-time validation.
//!
//! `derive(Table) #[hyperdb(register)]` calls into this registry at macro
//! expansion time to record:
//! - The SQL `CREATE TABLE` statement for the table (for lazy seeding).
//! - The struct's field-name list (for the name-subset diff in `validate.rs`).
//!
//! Tables are seeded into the `CompileTimeDb` lazily: only when a `query_as!`
//! dry-run returns SQLSTATE `42P01` (undefined_table) do we seed the relevant
//! table and retry. This handles cross-file macro expansion ordering without
//! requiring a client-side SQL parser.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

/// Information about a registered table derived from `#[derive(Table)]`.
#[derive(Debug, Clone)]
pub struct TableEntry {
    /// The SQL `CREATE TABLE` statement emitted by `derive(Table)`.
    pub create_sql: String,
    /// Struct field names that map to columns (honoring `#[hyperdb(rename)]`,
    /// excluding `#[hyperdb(index = N)]` fields).
    pub fields: Vec<String>,
}

/// Global registry: table name → entry. Populated by `derive(Table)` +
/// `#[hyperdb(register)]` at macro expansion time.
static REGISTRY: OnceLock<Mutex<HashMap<String, TableEntry>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, TableEntry>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a table and its associated struct field list.
///
/// Called by the `derive(Table) #[hyperdb(register)]` expansion.
/// `table_name` is the SQL table name (lower-snake-case unless overridden by
/// `#[hyperdb(table = "...")]`). `fields` are the column names the struct
/// expects in query results.
pub fn register(table_name: impl Into<String>, create_sql: impl Into<String>, fields: Vec<String>) {
    registry().lock().insert(
        table_name.into(),
        TableEntry {
            create_sql: create_sql.into(),
            fields,
        },
    );
}

/// Look up a registered table by name.
pub fn get(table_name: &str) -> Option<TableEntry> {
    registry().lock().get(table_name).cloned()
}

/// Returns true if the table name is known to the registry (regardless of
/// whether it has been seeded into the `CompileTimeDb` yet).
pub fn contains(table_name: &str) -> bool {
    registry().lock().contains_key(table_name)
}

/// All registered table names (for diagnostics).
pub fn registered_names() -> Vec<String> {
    registry().lock().keys().cloned().collect()
}

/// The public `Registry` type — a thin newtype that provides the seeding
/// interface against a live `CompileTimeDb`. Created from a lock guard by
/// `validate_query_as`.
#[derive(Debug)]
pub struct Registry;

impl Registry {
    /// Seed a registered table into `db` if it hasn't been created yet.
    ///
    /// Returns `true` if the table was seeded, `false` if unknown.
    ///
    /// # Errors
    ///
    /// Returns a Hyper error if the `CREATE TABLE` command fails.
    pub fn seed_if_known(
        table_name: &str,
        db: &mut crate::db::CompileTimeDb,
    ) -> hyperdb_api::Result<bool> {
        let Some(entry) = get(table_name) else {
            return Ok(false);
        };
        db.conn.execute_command(&entry.create_sql)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests use unique table-name prefixes so they don't share global state
    // and can run in parallel without races.

    #[test]
    fn register_and_retrieve() {
        register(
            "reg_test_users",
            "CREATE TABLE reg_test_users (id BIGINT, name TEXT)",
            vec!["id".into(), "name".into()],
        );
        let entry = get("reg_test_users").expect("just registered");
        assert_eq!(entry.fields, &["id", "name"]);
        assert!(entry.create_sql.contains("reg_test_users"));
    }

    #[test]
    fn contains_returns_false_for_unknown() {
        assert!(!contains("reg_test_nonexistent_xyzzy"));
    }

    #[test]
    fn registration_ordering_independent() {
        // Register B before A; both should be retrievable regardless of order.
        register(
            "reg_test_orders",
            "CREATE TABLE reg_test_orders (id BIGINT, user_id BIGINT)",
            vec!["id".into(), "user_id".into()],
        );
        register(
            "reg_test_customers",
            "CREATE TABLE reg_test_customers (id BIGINT)",
            vec!["id".into()],
        );
        assert!(contains("reg_test_orders"));
        assert!(contains("reg_test_customers"));
    }
}
