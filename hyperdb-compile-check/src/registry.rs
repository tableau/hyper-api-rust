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

/// Global registry: **both** table name and struct ident → entry.
///
/// Keyed by table name for the dry-run seed-and-retry path (Hyper reports the
/// SQL table name in 42P01 errors). Also indexed by struct name so that
/// `validate_query_as(struct_name, sql)` — which receives the Rust ident, not
/// the SQL name — can find the entry without knowing the table name upfront.
static REGISTRY: OnceLock<Mutex<HashMap<String, TableEntry>>> = OnceLock::new();

/// Reverse map: struct ident → SQL table name. Populated alongside REGISTRY.
static STRUCT_TO_TABLE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, TableEntry>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn struct_to_table() -> &'static Mutex<HashMap<String, String>> {
    STRUCT_TO_TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a table and its associated struct field list.
///
/// Called by the `derive(Table) #[hyperdb(register)]` expansion.
/// - `struct_name`: the Rust struct ident (e.g. `"User"`), used by
///   `validate_query_as` which receives the ident from `query_as!(User, …)`.
/// - `table_name`: the SQL table name (e.g. `"users"`), used when Hyper
///   reports a missing table via SQLSTATE 42P01.
/// - `fields`: column names the struct expects in query results.
pub fn register(
    struct_name: impl Into<String>,
    table_name: impl Into<String>,
    create_sql: impl Into<String>,
    fields: Vec<String>,
) {
    let struct_name = struct_name.into();
    let table_name = table_name.into();
    let entry = TableEntry {
        create_sql: create_sql.into(),
        fields,
    };
    registry().lock().insert(table_name.clone(), entry.clone());
    struct_to_table()
        .lock()
        .insert(struct_name, table_name.clone());
}

/// Look up a registered entry by **SQL table name**.
pub fn get_by_table(table_name: &str) -> Option<TableEntry> {
    registry().lock().get(table_name).cloned()
}

/// Look up a registered entry by **Rust struct ident**.
/// Returns `(table_name, entry)` so callers have the SQL name for seeding.
pub fn get_by_struct(struct_name: &str) -> Option<(String, TableEntry)> {
    let table_name = struct_to_table().lock().get(struct_name).cloned()?;
    let entry = registry().lock().get(&table_name).cloned()?;
    Some((table_name, entry))
}

/// Returns true if the **SQL table name** is known to the registry.
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
        let Some(entry) = get_by_table(table_name) else {
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
    fn register_and_retrieve_by_table() {
        register(
            "RegTestUser",
            "reg_test_users",
            "CREATE TABLE reg_test_users (id BIGINT, name TEXT)",
            vec!["id".into(), "name".into()],
        );
        let entry = get_by_table("reg_test_users").expect("lookup by table name");
        assert_eq!(entry.fields, &["id", "name"]);
        assert!(entry.create_sql.contains("reg_test_users"));
    }

    #[test]
    fn register_and_retrieve_by_struct() {
        register(
            "RegTestProfile",
            "reg_test_profiles",
            "CREATE TABLE reg_test_profiles (id BIGINT, bio TEXT)",
            vec!["id".into(), "bio".into()],
        );
        let (table_name, entry) = get_by_struct("RegTestProfile").expect("lookup by struct name");
        assert_eq!(table_name, "reg_test_profiles");
        assert_eq!(entry.fields, &["id", "bio"]);
    }

    #[test]
    fn contains_returns_false_for_unknown() {
        assert!(!contains("reg_test_nonexistent_xyzzy"));
    }

    #[test]
    fn registration_ordering_independent() {
        register(
            "RegTestOrder",
            "reg_test_orders",
            "CREATE TABLE reg_test_orders (id BIGINT, user_id BIGINT)",
            vec!["id".into(), "user_id".into()],
        );
        register(
            "RegTestCustomer",
            "reg_test_customers",
            "CREATE TABLE reg_test_customers (id BIGINT)",
            vec!["id".into()],
        );
        assert!(contains("reg_test_orders"));
        assert!(contains("reg_test_customers"));
        assert!(get_by_struct("RegTestOrder").is_some());
        assert!(get_by_struct("RegTestCustomer").is_some());
    }
}
