// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async key-value store — the [`AsyncConnection`] twin of [`KvStore`](crate::KvStore).

use crate::async_connection::AsyncConnection;
use crate::error::{Error, Result};
use crate::kv_store::{kv_create_table_sql, validate_kv_name, KV_TABLE};

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
    /// `target` is interpolated into SQL — the caller must supply a
    /// pre-validated / identifier-escaped, SQL-safe qualifier (M2 must escape
    /// it before calling).
    #[allow(
        dead_code,
        reason = "M2 (hyperdb-mcp) consumer; kept here so M1 needs no later API change"
    )]
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

    /// Sets `key` to `value` (upsert).
    ///
    /// # Errors
    ///
    /// See [`KvStore::set`](crate::KvStore::set).
    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        validate_kv_name(key, "key")?;
        self.upsert(key, value).await
    }

    /// UPDATE-then-conditional-INSERT upsert. Assumes `key` is validated.
    ///
    /// Mirrors [`KvStore::upsert`](crate::KvStore); the conditional INSERT uses
    /// distinct placeholders (`$4`/`$5`) so it is unambiguous under the
    /// extended-query protocol.
    async fn upsert(&self, key: &str, value: &str) -> Result<()> {
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
        Ok(())
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

    /// Serializes `value` to JSON and stores it under `key` (upsert).
    ///
    /// # Errors
    ///
    /// See [`KvStore::set_as`](crate::KvStore::set_as).
    pub async fn set_as<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        validate_kv_name(key, "key")?;
        let json = serde_json::to_string(value).map_err(|e| Error::serialization(e.to_string()))?;
        self.upsert(key, &json).await
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

    /// Upserts every `(key, value)` pair in one transaction.
    ///
    /// All keys are validated before the transaction opens, so an invalid key
    /// aborts the whole batch without writing anything.
    ///
    /// # Errors
    ///
    /// See [`KvStore::set_batch`](crate::KvStore::set_batch).
    pub async fn set_batch(&self, entries: &[(&str, &str)]) -> Result<()> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw().await?;
        let mut inner: Result<()> = Ok(());
        for (key, value) in entries {
            if let Err(e) = self.upsert(key, value).await {
                inner = Err(e);
                break;
            }
        }
        match &inner {
            Ok(()) => self.connection.commit_raw().await?,
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

    /// Lists the names of every KV store that currently holds at least one key.
    ///
    /// # Errors
    ///
    /// See [`Connection::kv_list_stores`](crate::Connection::kv_list_stores).
    pub async fn kv_list_stores(&self) -> Result<Vec<String>> {
        self.execute_command(&kv_create_table_sql(KV_TABLE)).await?;
        let mut result = self
            .execute_query(&format!(
                "SELECT DISTINCT store_name FROM {KV_TABLE} ORDER BY store_name ASC"
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
