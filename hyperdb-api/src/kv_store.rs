// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Key-value store over a fixed Hyper table.
//!
//! [`KvStore`] is an ergonomic string-native KV abstraction backed by a
//! single table, `KV_TABLE`, namespaced by a `store_name` column. Every
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

/// Builds the escaped `"<database>"."public"` qualifier prefix for the KV table.
///
/// Single source of truth for the location shape used by
/// [`Connection::kv_store_in`](crate::Connection::kv_store_in) and
/// [`Connection::kv_list_stores_in`](crate::Connection::kv_list_stores_in) (and
/// their async twins): the KV table always lives in the target database's
/// `public` schema. `database` is identifier-escaped via
/// [`escape_name`](crate::escape_name); the fixed `public` schema name is
/// escaped identically for symmetry.
pub(crate) fn kv_target_prefix(database: &str) -> Result<String> {
    Ok(format!(
        "{}.{}",
        crate::escape_name(database)?,
        crate::escape_name("public")?
    ))
}

use crate::connection::Connection;

/// A handle to one named key-value store, backed by `KV_TABLE`.
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
    /// Opens a handle to `name`, creating `KV_TABLE` if needed.
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

    /// Opens a handle targeting an explicit, already-escaped table-qualifier prefix.
    ///
    /// Crate-internal low-level constructor behind
    /// [`Connection::kv_store_in`](crate::Connection::kv_store_in). `target` is
    /// interpolated directly into SQL, so the **caller must supply a pre-escaped,
    /// SQL-safe qualifier** — public callers go through `kv_store_in`, which
    /// escapes for them (`store_name` / `key` / `value` are always bound params,
    /// but `target` is not).
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

    /// Returns the value for `key`, or `None` if the key is absent or NULL.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the query fails.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        validate_kv_name(key, "key")?;
        let sql = format!(
            "SELECT value FROM {} WHERE store_name = $1 AND key = $2",
            self.table_ref
        );
        // Bind store_name/key as `&str` params (never interpolated) — uniform
        // `&str` element types coerce cleanly to `&[&dyn ToSqlParam]`.
        let row = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str(), &key])?
            .first_row()?;
        Ok(row.and_then(|r| r.get::<String>(0)))
    }

    /// Sets `key` to `value`, inserting or overwriting (upsert).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the `UPDATE`/`INSERT` fails.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        validate_kv_name(key, "key")?;
        self.upsert(key, value)
    }

    /// UPDATE-then-conditional-INSERT upsert. Assumes `key` is validated.
    ///
    /// Hyper has no `ON CONFLICT`; this mirrors the proven `_table_catalog`
    /// idiom. The conditional INSERT uses distinct placeholders (`$4`/`$5`)
    /// so it is unambiguous under the extended-query protocol.
    fn upsert(&self, key: &str, value: &str) -> Result<()> {
        let store = self.store_name.as_str();
        let updated = self.connection.command_params(
            &format!(
                "UPDATE {} SET value = $3 WHERE store_name = $1 AND key = $2",
                self.table_ref
            ),
            &[&store, &key, &value],
        )?;
        if updated == 0 {
            self.connection.command_params(
                &format!(
                    "INSERT INTO {t} (store_name, key, value) \
                     SELECT $1, $2, $3 \
                     WHERE NOT EXISTS (SELECT 1 FROM {t} WHERE store_name = $4 AND key = $5)",
                    t = self.table_ref
                ),
                &[&store, &key, &value, &store, &key],
            )?;
        }
        Ok(())
    }

    /// Deserializes the JSON-encoded value for `key` into `T`.
    ///
    /// Returns `None` if the key is absent.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::Serialization`] if the stored value is not valid JSON for `T`.
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`] as for [`get`](Self::get).
    pub fn get_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key)? {
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| Error::serialization(e.to_string())),
            None => Ok(None),
        }
    }

    /// Serializes `value` to JSON and stores it under `key` (upsert).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::Serialization`] if `value` cannot be serialized to JSON.
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`] as for [`set`](Self::set).
    pub fn set_as<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        validate_kv_name(key, "key")?;
        let json = serde_json::to_string(value).map_err(|e| Error::serialization(e.to_string()))?;
        self.upsert(key, &json)
    }

    /// Deletes `key`; returns `true` if a row was removed.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn delete(&self, key: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let affected = self.connection.command_params(
            &format!(
                "DELETE FROM {} WHERE store_name = $1 AND key = $2",
                self.table_ref
            ),
            &[&self.store_name.as_str(), &key],
        )?;
        Ok(affected > 0)
    }

    /// Returns whether `key` is present in this store.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn exists(&self, key: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let sql = format!(
            "SELECT 1 FROM {} WHERE store_name = $1 AND key = $2 LIMIT 1",
            self.table_ref
        );
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str(), &key])?
            .first_row()?
            .is_some())
    }

    /// Returns the number of keys in this store.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn size(&self) -> Result<i64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE store_name = $1",
            self.table_ref
        );
        // `scalar()` errors on zero rows, but COUNT(*) always returns exactly
        // one non-NULL row, so `unwrap_or(0)` is unreachable-but-defensive.
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])?
            .scalar::<i64>()?
            .unwrap_or(0))
    }

    /// Returns this store's keys, sorted ascending.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn keys(&self) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT key FROM {} WHERE store_name = $1 ORDER BY key ASC",
            self.table_ref
        );
        let mut result = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])?;
        let mut keys = Vec::new();
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                if let Some(k) = row.get::<String>(0) {
                    keys.push(k);
                }
            }
        }
        Ok(keys)
    }

    /// Deletes every key in this store; returns the number removed.
    ///
    /// The shared backing table survives; only this store's rows are removed.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn clear(&self) -> Result<u64> {
        self.connection.command_params(
            &format!("DELETE FROM {} WHERE store_name = $1", self.table_ref),
            &[&self.store_name.as_str()],
        )
    }

    /// Removes and returns the lowest-ordered key/value pair, or `None` if empty.
    ///
    /// The peek and delete run in one transaction, so they apply atomically —
    /// either both the read and the delete commit, or neither does (on error
    /// the transaction is rolled back). A SQL-NULL value is returned as an
    /// empty string.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn pop(&self) -> Result<Option<(String, String)>> {
        self.connection.begin_transaction_raw()?;
        let result = self.pop_inner();
        match &result {
            Ok(_) => self.connection.commit_raw()?,
            Err(_) => {
                // Best-effort rollback; preserve the original error.
                let _ = self.connection.rollback_raw();
            }
        }
        result
    }

    /// Transaction body for [`pop`](Self::pop).
    fn pop_inner(&self) -> Result<Option<(String, String)>> {
        let store = self.store_name.as_str();
        let select = format!(
            "SELECT key, value FROM {} WHERE store_name = $1 ORDER BY key ASC LIMIT 1",
            self.table_ref
        );
        // `first_row()` consumes the `Rowset`, dropping it (and releasing its
        // statement guard on the shared connection) BEFORE the DELETE runs —
        // the two statements never overlap on the connection.
        let Some(row) = self
            .connection
            .query_params(&select, &[&store])?
            .first_row()?
        else {
            return Ok(None);
        };
        let key: String = row
            .get::<String>(0)
            .ok_or_else(|| Error::internal("kv pop: key column was unexpectedly NULL"))?;
        let value: String = row.get::<String>(1).unwrap_or_default();
        self.connection.command_params(
            &format!(
                "DELETE FROM {} WHERE store_name = $1 AND key = $2",
                self.table_ref
            ),
            &[&store, &key.as_str()],
        )?;
        Ok(Some((key, value)))
    }

    /// Upserts every `(key, value)` pair in one transaction.
    ///
    /// All keys are validated before the transaction opens, so an invalid key
    /// aborts the whole batch without writing anything.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if any key is invalid (checked before writing).
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn set_batch(&self, entries: &[(&str, &str)]) -> Result<()> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw()?;
        let result = (|| {
            for (key, value) in entries {
                self.upsert(key, value)?;
            }
            Ok(())
        })();
        match &result {
            Ok(()) => self.connection.commit_raw()?,
            Err(_) => {
                let _ = self.connection.rollback_raw();
            }
        }
        result
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

    /// Opens a handle to a KV store in a specific database, rather than the
    /// default (search-path) location.
    ///
    /// `database` is the **unescaped** name of an attached database; the store's
    /// backing table is created (if absent) in that database's `public` schema.
    /// The name is identifier-escaped internally, so it is safe to pass an
    /// arbitrary attachment alias. The store name, keys, and values are always
    /// bound parameters.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, CreateMode, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let kv = conn.kv_store_in("persistent", "settings")?;
    /// kv.set("theme", "dark")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `database` or `name` is invalid.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if creating the backing table fails.
    pub fn kv_store_in(&self, database: &str, name: &str) -> Result<KvStore<'_>> {
        KvStore::with_target(self, name, &kv_target_prefix(database)?)
    }

    /// Lists the names of every KV store that currently holds at least one key.
    ///
    /// Creates the backing table first (via `kv_create_table_sql`) so calling
    /// this on a fresh database returns an empty list rather than erroring on a
    /// missing table.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the query fails.
    pub fn kv_list_stores(&self) -> Result<Vec<String>> {
        self.kv_list_stores_impl(KV_TABLE)
    }

    /// Lists the KV stores that hold at least one key in a specific database.
    ///
    /// The location-aware companion to [`kv_list_stores`](Self::kv_list_stores):
    /// `database` is the unescaped name of an attached database (escaped
    /// internally). Creates the backing table in that database's `public` schema
    /// first, so an empty database returns `[]` rather than erroring.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `database` is invalid.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the query fails.
    pub fn kv_list_stores_in(&self, database: &str) -> Result<Vec<String>> {
        let table_ref = format!("{}.{KV_TABLE}", kv_target_prefix(database)?);
        self.kv_list_stores_impl(&table_ref)
    }

    /// Shared body for [`kv_list_stores`](Self::kv_list_stores) /
    /// [`kv_list_stores_in`](Self::kv_list_stores_in): create the backing table
    /// at `table_ref` (so a fresh location returns `[]`), then list the distinct
    /// store names there. Factored out so the default and location-aware paths
    /// cannot drift.
    fn kv_list_stores_impl(&self, table_ref: &str) -> Result<Vec<String>> {
        self.execute_command(&kv_create_table_sql(table_ref))?;
        let mut result = self.execute_query(&format!(
            "SELECT DISTINCT store_name FROM {table_ref} ORDER BY store_name ASC"
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
