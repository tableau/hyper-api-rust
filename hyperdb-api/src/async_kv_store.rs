// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async key-value store — the [`AsyncConnection`] twin of [`KvStore`](crate::KvStore).

use crate::async_connection::AsyncConnection;
use crate::error::{Error, Result};
use crate::kv_store::{
    kv_create_table_sql, kv_target_prefix, validate_kv_name, BatchGuardOutcome, BatchSetOutcome,
    SetOutcome, KV_TABLE,
};

/// A handle to one named key-value store over an [`AsyncConnection`].
///
/// The async twin of [`KvStore`](crate::KvStore); see it for semantics. Open
/// one with [`AsyncConnection::kv_store`].
///
/// # Examples
///
/// ```no_run
/// use hyperdb_api::{AsyncConnection, CreateMode, Result};
///
/// async fn demo(conn: &AsyncConnection) -> Result<()> {
///     let kv = conn.kv_store("settings").await?;
///     kv.set("theme", "dark").await?;
///     assert_eq!(kv.get("theme").await?, Some("dark".to_string()));
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct AsyncKvStore<'conn> {
    connection: &'conn AsyncConnection,
    store_name: String,
    table_ref: String,
}

impl<'conn> AsyncKvStore<'conn> {
    /// Opens a handle to `name`, creating `KV_TABLE` if needed.
    async fn open(
        connection: &'conn AsyncConnection,
        name: &str,
        table_ref: String,
    ) -> Result<Self> {
        validate_kv_name(name, "store name")?;
        connection
            .execute_command(&kv_create_table_sql(&table_ref))
            .await?;
        Ok(AsyncKvStore {
            connection,
            store_name: name.to_string(),
            table_ref,
        })
    }

    /// Opens a handle to a store in the default location.
    pub(crate) async fn new(connection: &'conn AsyncConnection, name: &str) -> Result<Self> {
        Self::open(connection, name, KV_TABLE.to_string()).await
    }

    /// Async twin of [`KvStore::with_target`](crate::KvStore::with_target).
    ///
    /// Crate-internal low-level constructor behind
    /// [`AsyncConnection::kv_store_in`](crate::AsyncConnection::kv_store_in).
    /// `target` is interpolated into SQL — the caller must supply a pre-escaped,
    /// SQL-safe qualifier (public callers go through `kv_store_in`, which
    /// escapes for them).
    pub(crate) async fn with_target(
        connection: &'conn AsyncConnection,
        name: &str,
        target: &str,
    ) -> Result<Self> {
        Self::open(connection, name, format!("{target}.{KV_TABLE}")).await
    }

    /// Returns this store's validated name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.store_name
    }

    /// Returns the value for `key`, or `None` if absent or NULL.
    ///
    /// # Errors
    ///
    /// See [`KvStore::get`](crate::KvStore::get).
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        validate_kv_name(key, "key")?;
        let sql = format!(
            "SELECT value FROM {} WHERE store_name = $1 AND key = $2",
            self.table_ref
        );
        // Bind store_name/key as `&str` params (never interpolated) — uniform
        // `&str` element types coerce cleanly to `&[&dyn ToSqlParam]`.
        let row = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str(), &key])
            .await?
            .first_row()
            .await?;
        Ok(row.and_then(|r| r.get::<String>(0)))
    }

    /// Sets `key` to `value`, inserting or overwriting (upsert). Returns
    /// [`SetOutcome`] indicating whether the key was newly created.
    ///
    /// # Errors
    ///
    /// See [`KvStore::set`](crate::KvStore::set).
    pub async fn set(&self, key: &str, value: &str) -> Result<SetOutcome> {
        validate_kv_name(key, "key")?;
        Ok(SetOutcome {
            created: self.upsert(key, value).await?,
        })
    }

    /// UPDATE-then-conditional-INSERT upsert. Assumes `key` is validated.
    /// Returns `true` if the row was newly inserted (created), `false` if an
    /// existing value was overwritten.
    ///
    /// Mirrors [`KvStore::upsert`](crate::KvStore); the conditional INSERT uses
    /// distinct placeholders (`$4`/`$5`) so it is unambiguous under the
    /// extended-query protocol.
    async fn upsert(&self, key: &str, value: &str) -> Result<bool> {
        let updated = self
            .connection
            .command_params(
                &format!(
                    "UPDATE {} SET value = $3 WHERE store_name = $1 AND key = $2",
                    self.table_ref
                ),
                &[&self.store_name.as_str(), &key, &value],
            )
            .await?;
        if updated == 0 {
            self.connection
                .command_params(
                    &format!(
                        "INSERT INTO {t} (store_name, key, value) \
                         SELECT $1, $2, $3 \
                         WHERE NOT EXISTS (SELECT 1 FROM {t} WHERE store_name = $4 AND key = $5)",
                        t = self.table_ref
                    ),
                    &[
                        &self.store_name.as_str(),
                        &key,
                        &value,
                        &self.store_name.as_str(),
                        &key,
                    ],
                )
                .await?;
        }
        Ok(updated == 0)
    }

    /// Deserializes the JSON value for `key` into `T`; `None` if absent.
    ///
    /// # Errors
    ///
    /// See [`KvStore::get_as`](crate::KvStore::get_as).
    pub async fn get_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key).await? {
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| Error::serialization(e.to_string())),
            None => Ok(None),
        }
    }

    /// Serializes `value` to JSON and stores it under `key` (upsert). Returns
    /// [`SetOutcome`] indicating whether the key was newly created.
    ///
    /// # Errors
    ///
    /// See [`KvStore::set_as`](crate::KvStore::set_as).
    pub async fn set_as<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<SetOutcome> {
        validate_kv_name(key, "key")?;
        let json = serde_json::to_string(value).map_err(|e| Error::serialization(e.to_string()))?;
        Ok(SetOutcome {
            created: self.upsert(key, &json).await?,
        })
    }

    /// Inserts `value` under `key` only if `key` is absent.
    ///
    /// Returns `true` if a row was written, `false` if the key already existed
    /// (in which case nothing is written). A single `INSERT ... WHERE NOT
    /// EXISTS` statement decides, so there is no check-then-write race.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the `INSERT` fails.
    pub async fn set_if_absent(&self, key: &str, value: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let store = self.store_name.as_str();
        let inserted = self
            .connection
            .command_params(
                &format!(
                    "INSERT INTO {t} (store_name, key, value) \
                     SELECT $1, $2, $3 \
                     WHERE NOT EXISTS (SELECT 1 FROM {t} WHERE store_name = $4 AND key = $5)",
                    t = self.table_ref
                ),
                &[&store, &key, &value, &store, &key],
            )
            .await?;
        Ok(inserted > 0)
    }

    /// Deletes `key`; returns `true` if a row was removed.
    ///
    /// # Errors
    ///
    /// See [`KvStore::delete`](crate::KvStore::delete).
    pub async fn delete(&self, key: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let affected = self
            .connection
            .command_params(
                &format!(
                    "DELETE FROM {} WHERE store_name = $1 AND key = $2",
                    self.table_ref
                ),
                &[&self.store_name.as_str(), &key],
            )
            .await?;
        Ok(affected > 0)
    }

    /// Returns whether `key` is present.
    ///
    /// # Errors
    ///
    /// See [`KvStore::exists`](crate::KvStore::exists).
    pub async fn exists(&self, key: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let sql = format!(
            "SELECT 1 FROM {} WHERE store_name = $1 AND key = $2 LIMIT 1",
            self.table_ref
        );
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str(), &key])
            .await?
            .first_row()
            .await?
            .is_some())
    }

    /// Returns the number of keys in this store.
    ///
    /// # Errors
    ///
    /// See [`KvStore::size`](crate::KvStore::size).
    pub async fn size(&self) -> Result<i64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE store_name = $1",
            self.table_ref
        );
        // `scalar()` errors on zero rows, but COUNT(*) always returns exactly
        // one non-NULL row, so `unwrap_or(0)` is unreachable-but-defensive.
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])
            .await?
            .scalar::<i64>()
            .await?
            .unwrap_or(0))
    }

    /// Returns the total byte size of all values in this store (0 if empty).
    ///
    /// # Errors
    ///
    /// See [`KvStore::byte_size`](crate::KvStore::byte_size).
    pub async fn byte_size(&self) -> Result<i64> {
        let sql = format!(
            "SELECT COALESCE(SUM(OCTET_LENGTH(value)), 0) FROM {} WHERE store_name = $1",
            self.table_ref
        );
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])
            .await?
            .scalar::<i64>()
            .await?
            .unwrap_or(0))
    }

    /// Returns this store's `(key, value)` pairs, sorted by key ascending.
    ///
    /// Materializes the whole store — intended for small scratchpad stores.
    ///
    /// # Errors
    ///
    /// See [`KvStore::entries`](crate::KvStore::entries).
    pub async fn entries(&self) -> Result<Vec<(String, String)>> {
        let sql = format!(
            "SELECT key, value FROM {} WHERE store_name = $1 ORDER BY key ASC",
            self.table_ref
        );
        let mut result = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])
            .await?;
        let mut entries = Vec::new();
        while let Some(chunk) = result.next_chunk().await? {
            for row in &chunk {
                if let Some(k) = row.get::<String>(0) {
                    entries.push((k, row.get::<String>(1).unwrap_or_default()));
                }
            }
        }
        Ok(entries)
    }

    /// Returns this store's keys, sorted ascending.
    ///
    /// # Errors
    ///
    /// See [`KvStore::keys`](crate::KvStore::keys).
    pub async fn keys(&self) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT key FROM {} WHERE store_name = $1 ORDER BY key ASC",
            self.table_ref
        );
        let mut result = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])
            .await?;
        let mut keys = Vec::new();
        while let Some(chunk) = result.next_chunk().await? {
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
    /// See [`KvStore::clear`](crate::KvStore::clear).
    pub async fn clear(&self) -> Result<u64> {
        self.connection
            .command_params(
                &format!("DELETE FROM {} WHERE store_name = $1", self.table_ref),
                &[&self.store_name.as_str()],
            )
            .await
    }

    /// Removes and returns the lowest-ordered pair, or `None` if empty.
    ///
    /// The peek and delete run in one transaction, so they apply atomically —
    /// either both commit, or neither does (on error the transaction is rolled
    /// back). A SQL-NULL value is returned as an empty string.
    ///
    /// # Errors
    ///
    /// See [`KvStore::pop`](crate::KvStore::pop).
    pub async fn pop(&self) -> Result<Option<(String, String)>> {
        self.connection.begin_transaction_raw().await?;
        let result = self.pop_inner().await;
        match &result {
            Ok(_) => self.connection.commit_raw().await?,
            Err(_) => {
                // Best-effort rollback; preserve the original error.
                let _ = self.connection.rollback_raw().await;
            }
        }
        result
    }

    /// Transaction body for [`pop`](Self::pop).
    async fn pop_inner(&self) -> Result<Option<(String, String)>> {
        let select = format!(
            "SELECT key, value FROM {} WHERE store_name = $1 ORDER BY key ASC LIMIT 1",
            self.table_ref
        );
        // `first_row()` consumes the `AsyncRowset`, releasing its statement
        // guard on the shared connection BEFORE the DELETE runs — the two
        // statements never overlap on the connection.
        let Some(row) = self
            .connection
            .query_params(&select, &[&self.store_name.as_str()])
            .await?
            .first_row()
            .await?
        else {
            return Ok(None);
        };
        let key: String = row
            .get::<String>(0)
            .ok_or_else(|| Error::internal("kv pop: key column was unexpectedly NULL"))?;
        let value: String = row.get::<String>(1).unwrap_or_default();
        self.connection
            .command_params(
                &format!(
                    "DELETE FROM {} WHERE store_name = $1 AND key = $2",
                    self.table_ref
                ),
                &[&self.store_name.as_str(), &key.as_str()],
            )
            .await?;
        Ok(Some((key, value)))
    }

    /// Upserts every `(key, value)` pair in one transaction. Returns
    /// [`BatchSetOutcome`] reporting how many keys were newly inserted vs.
    /// overwritten.
    ///
    /// All keys are validated before the transaction opens, so an invalid key
    /// aborts the whole batch without writing anything.
    ///
    /// # Errors
    ///
    /// See [`KvStore::set_batch`](crate::KvStore::set_batch).
    pub async fn set_batch(&self, entries: &[(&str, &str)]) -> Result<BatchSetOutcome> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw().await?;
        let result = async {
            let mut outcome = BatchSetOutcome {
                created: 0,
                overwritten: 0,
            };
            for (key, value) in entries {
                if self.upsert(key, value).await? {
                    outcome.created += 1;
                } else {
                    outcome.overwritten += 1;
                }
            }
            Ok(outcome)
        }
        .await;
        match &result {
            Ok(_) => self.connection.commit_raw().await?,
            Err(_) => {
                let _ = self.connection.rollback_raw().await;
            }
        }
        result
    }

    /// Inserts every absent `(key, value)` pair in one transaction, skipping
    /// keys that already exist. All keys are validated before the transaction
    /// opens, so an invalid key aborts the whole batch without writing anything.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if any key is invalid (checked before writing).
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub async fn set_batch_if_absent(&self, entries: &[(&str, &str)]) -> Result<BatchGuardOutcome> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw().await?;
        let mut inner: Result<BatchGuardOutcome> = Ok(BatchGuardOutcome {
            written: 0,
            skipped: 0,
        });
        for (key, value) in entries {
            match self.set_if_absent(key, value).await {
                Ok(true) => {
                    if let Ok(o) = inner.as_mut() {
                        o.written += 1;
                    }
                }
                Ok(false) => {
                    if let Ok(o) = inner.as_mut() {
                        o.skipped += 1;
                    }
                }
                Err(e) => {
                    inner = Err(e);
                    break;
                }
            }
        }
        match &inner {
            Ok(_) => self.connection.commit_raw().await?,
            Err(_) => {
                let _ = self.connection.rollback_raw().await;
            }
        }
        inner
    }
}

impl AsyncConnection {
    /// Opens a handle to a named KV store, creating the table if needed.
    ///
    /// # Errors
    ///
    /// See [`Connection::kv_store`](crate::Connection::kv_store).
    pub async fn kv_store(&self, name: &str) -> Result<AsyncKvStore<'_>> {
        AsyncKvStore::new(self, name).await
    }

    /// Async twin of [`Connection::kv_store_in`](crate::Connection::kv_store_in).
    ///
    /// # Errors
    ///
    /// See [`Connection::kv_store_in`](crate::Connection::kv_store_in).
    pub async fn kv_store_in(&self, database: &str, name: &str) -> Result<AsyncKvStore<'_>> {
        AsyncKvStore::with_target(self, name, &kv_target_prefix(database)?).await
    }

    /// Lists the names of every KV store that currently holds at least one key.
    ///
    /// # Errors
    ///
    /// See [`Connection::kv_list_stores`](crate::Connection::kv_list_stores).
    pub async fn kv_list_stores(&self) -> Result<Vec<String>> {
        self.kv_list_stores_impl(KV_TABLE).await
    }

    /// Async twin of
    /// [`Connection::kv_list_stores_in`](crate::Connection::kv_list_stores_in).
    ///
    /// # Errors
    ///
    /// See [`Connection::kv_list_stores_in`](crate::Connection::kv_list_stores_in).
    pub async fn kv_list_stores_in(&self, database: &str) -> Result<Vec<String>> {
        let table_ref = format!("{}.{KV_TABLE}", kv_target_prefix(database)?);
        self.kv_list_stores_impl(&table_ref).await
    }

    /// Shared body for [`kv_list_stores`](Self::kv_list_stores) /
    /// [`kv_list_stores_in`](Self::kv_list_stores_in): the async twin of the sync
    /// `kv_list_stores_impl`. Factored out so the default and location-aware
    /// paths cannot drift.
    async fn kv_list_stores_impl(&self, table_ref: &str) -> Result<Vec<String>> {
        self.execute_command(&kv_create_table_sql(table_ref))
            .await?;
        let mut result = self
            .execute_query(&format!(
                "SELECT DISTINCT store_name FROM {table_ref} ORDER BY store_name ASC"
            ))
            .await?;
        let mut names = Vec::new();
        while let Some(chunk) = result.next_chunk().await? {
            for row in &chunk {
                if let Some(name) = row.get::<String>(0) {
                    names.push(name);
                }
            }
        }
        Ok(names)
    }
}
