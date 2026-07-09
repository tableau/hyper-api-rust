// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Key-value store over a fixed Hyper table.
//!
//! [`KvStore`] is an ergonomic string-native KV abstraction backed by a
//! single table, [`KV_TABLE`], namespaced by a `store_name` column. Every
//! named store shares that table; a handle binds one store name, validated
//! once at [`Connection::kv_store`](crate::Connection::kv_store).
//!
//! Hyper has no native KV store and no `ON CONFLICT`/`MERGE`; `set` is an
//! `UPDATE`-then-conditional-`INSERT` upsert. See the crate `DEVELOPMENT.md`
//! for the design rationale.

use crate::error::{Error, Result};

/// Fixed backing table for every named KV store.
///
/// The `_hyperdb_` prefix matches the crate's internal-table convention so
/// downstream tooling can auto-hide it from schema listings.
pub(crate) const KV_TABLE: &str = "_hyperdb_kv_store";

/// Maximum length, in bytes, of a store name or key.
pub(crate) const KV_MAX_NAME_BYTES: usize = 512;

/// Human-readable description of the allowed store-name/key charset.
///
/// Used in validation error messages so the allowed set is stated in one
/// place (M-DOCUMENTED-MAGIC) rather than duplicated as a string literal.
pub(crate) const KV_CHARSET: &str = "A-Z a-z 0-9 _ . -";

/// Validates a store name or key: non-empty, ASCII `[A-Za-z0-9_.-]+`, `<= 512` bytes.
///
/// `kind` labels the value in the error message (`"store name"` / `"key"`).
///
/// # Errors
///
/// Returns [`Error::InvalidName`] if `name` is empty, exceeds
/// [`KV_MAX_NAME_BYTES`] bytes, or contains a byte outside the ASCII
/// [`KV_CHARSET`] (`A-Z a-z 0-9 _ . -`).
pub(crate) fn validate_kv_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid_name(format!("KV {kind} must not be empty")));
    }
    if name.len() > KV_MAX_NAME_BYTES {
        return Err(Error::invalid_name(format!(
            "KV {kind} exceeds {KV_MAX_NAME_BYTES}-byte limit ({} bytes)",
            name.len()
        )));
    }
    if let Some(bad) = name
        .bytes()
        .find(|&b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-'))
    {
        return Err(Error::invalid_name(format!(
            "KV {kind} contains an invalid byte {bad:#04x}; allowed: {KV_CHARSET}"
        )));
    }
    Ok(())
}

/// Builds the `CREATE TABLE IF NOT EXISTS` DDL for the KV backing table.
///
/// Single source of truth for the schema, shared by the sync and async
/// constructors and both `kv_list_stores` guards. The table has **no**
/// `PRIMARY KEY`: Hyper rejects one at create time (`0A000: Index support is
/// disabled`, see `hyperdb-mcp/src/table_catalog.rs`), so per-`(store_name,
/// key)` uniqueness is an application-side invariant enforced by the
/// UPDATE-then-conditional-INSERT upsert, not an engine constraint.
pub(crate) fn kv_create_table_sql(table_ref: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table_ref} \
         (store_name TEXT NOT NULL, key TEXT NOT NULL, value TEXT)"
    )
}

use crate::connection::Connection;

/// A handle to one named key-value store, backed by [`KV_TABLE`].
///
/// Borrows its [`Connection`] for the handle's lifetime (`'conn`), matching
/// the crate's [`Catalog`](crate::Catalog)/[`Inserter`](crate::Inserter)
/// borrow convention. Open one with
/// [`Connection::kv_store`](crate::Connection::kv_store).
///
/// # Examples
///
/// ```no_run
/// use hyperdb_api::{Connection, CreateMode, Result};
///
/// fn main() -> Result<()> {
///     let conn = Connection::connect("localhost:7483", "app.hyper", CreateMode::CreateIfNotExists)?;
///     let kv = conn.kv_store("settings")?;
///     kv.set("theme", "dark")?;
///     assert_eq!(kv.get("theme")?, Some("dark".to_string()));
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct KvStore<'conn> {
    connection: &'conn Connection,
    store_name: String,
    table_ref: String,
}

impl<'conn> KvStore<'conn> {
    /// Opens a handle to `name`, creating [`KV_TABLE`] if needed.
    fn open(connection: &'conn Connection, name: &str, table_ref: String) -> Result<Self> {
        validate_kv_name(name, "store name")?;
        connection.execute_command(&kv_create_table_sql(&table_ref))?;
        Ok(KvStore {
            connection,
            store_name: name.to_string(),
            table_ref,
        })
    }

    /// Opens a handle to a store in the default location.
    pub(crate) fn new(connection: &'conn Connection, name: &str) -> Result<Self> {
        Self::open(connection, name, KV_TABLE.to_string())
    }

    /// Opens a handle targeting an explicit, already-escaped table reference.
    ///
    /// Crate-internal seam for the MCP milestone (routes into an attached
    /// database). `target` is interpolated directly into SQL, so the **caller
    /// must supply a pre-validated / identifier-escaped, SQL-safe qualifier**
    /// (M2 must escape it via the crate's identifier-quoting before calling —
    /// `store_name`/`key`/`value` are always bound params, but `target` is not).
    #[allow(
        dead_code,
        reason = "M2 (hyperdb-mcp) consumer; kept here so M1 needs no later API change"
    )]
    pub(crate) fn with_target(
        connection: &'conn Connection,
        name: &str,
        target: &str,
    ) -> Result<Self> {
        Self::open(connection, name, format!("{target}.{KV_TABLE}"))
    }

    /// Returns this store's validated name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.store_name
    }
}

impl Connection {
    /// Opens a handle to a named key-value store, creating the table if needed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, CreateMode, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let kv = conn.kv_store("session")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `name` is empty, too long, or has invalid characters.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the `CREATE TABLE IF NOT EXISTS` fails.
    pub fn kv_store(&self, name: &str) -> Result<KvStore<'_>> {
        KvStore::new(self, name)
    }

    /// Lists the names of every KV store that currently holds at least one key.
    ///
    /// Creates the backing table first (via [`kv_create_table_sql`]) so calling
    /// this on a fresh database returns an empty list rather than erroring on a
    /// missing table.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the query fails.
    pub fn kv_list_stores(&self) -> Result<Vec<String>> {
        self.execute_command(&kv_create_table_sql(KV_TABLE))?;
        let mut result = self.execute_query(&format!(
            "SELECT DISTINCT store_name FROM {KV_TABLE} ORDER BY store_name ASC"
        ))?;
        let mut names = Vec::new();
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                if let Some(name) = row.get::<String>(0) {
                    names.push(name);
                }
            }
        }
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for ok in [
            "a",
            "store_1",
            "my.key-2",
            "A",
            &"z".repeat(KV_MAX_NAME_BYTES),
        ] {
            assert!(validate_kv_name(ok, "key").is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn rejects_empty() {
        let err = validate_kv_name("", "store name").unwrap_err();
        assert!(matches!(err, Error::InvalidName(_)));
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(KV_MAX_NAME_BYTES + 1);
        let err = validate_kv_name(&long, "key").unwrap_err();
        assert!(matches!(err, Error::InvalidName(_)));
        assert!(err.to_string().contains("byte limit"));
    }

    #[test]
    fn rejects_bad_charset() {
        for bad in ["a b", "a/b", "a'b", "a\"b", "a;b", "naïve", "a\0b"] {
            let err = validate_kv_name(bad, "key").unwrap_err();
            assert!(
                matches!(err, Error::InvalidName(_)),
                "should reject {bad:?}"
            );
        }
    }
}
