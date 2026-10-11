// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! RAII transaction guard for async connections.

use crate::async_connection::AsyncConnection;
use crate::async_result::AsyncRowset;
use crate::error::Result;
use crate::result::{Row, RowValue};

/// An async RAII transaction guard.
///
/// Created via [`AsyncConnection::transaction()`]. This **exclusively borrows** the
/// connection for the lifetime of the transaction, preventing any other code from
/// using the raw connection while the transaction is active. Always explicitly call
/// [`commit()`](Self::commit) or [`rollback()`](Self::rollback) — the `Drop`
/// implementation cannot issue async rollback and will only emit a warning.
///
/// # Example
///
/// ```no_run
/// # use hyperdb_api::{AsyncConnection, CreateMode, Result};
/// # async fn example() -> Result<()> {
/// # let mut conn = AsyncConnection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate).await?;
/// let txn = conn.transaction().await?;
/// txn.execute_command("INSERT INTO users VALUES (1, 'Alice')").await?;
/// txn.commit().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct AsyncTransaction<'conn> {
    connection: &'conn mut AsyncConnection,
    completed: bool,
}

impl<'conn> AsyncTransaction<'conn> {
    /// Creates a new async transaction by issuing `BEGIN TRANSACTION`.
    pub(crate) async fn new(connection: &'conn mut AsyncConnection) -> Result<Self> {
        // Delegate to the public `*_unguarded` primitive; commit/rollback
        // consume `self`. Drop cannot roll back (no async Drop), so it only
        // warns.
        connection.begin_transaction_unguarded().await?;
        Ok(Self {
            connection,
            completed: false,
        })
    }

    /// Commits the transaction.
    ///
    /// # Errors
    ///
    /// Forwards the error from the server's `COMMIT`. The transaction
    /// is marked completed regardless, so the drop guard will not warn.
    pub async fn commit(mut self) -> Result<()> {
        self.completed = true;
        self.connection.commit_unguarded().await
    }

    /// Rolls back the transaction explicitly.
    ///
    /// # Errors
    ///
    /// Forwards the error from the server's `ROLLBACK`. The
    /// transaction is marked completed regardless.
    pub async fn rollback(mut self) -> Result<()> {
        self.completed = true;
        self.connection.rollback_unguarded().await
    }

    /// Returns a reference to the underlying async connection.
    #[must_use]
    pub fn connection(&self) -> &AsyncConnection {
        self.connection
    }

    // =========================================================================
    // Delegated execution methods
    // =========================================================================

    /// Executes a SQL command within this transaction.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::execute_command`].
    pub async fn execute_command(&self, sql: &str) -> Result<u64> {
        self.connection.execute_command(sql).await
    }

    /// Executes a streaming query within this transaction.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::execute_query`].
    pub async fn execute_query(&self, query: &str) -> Result<AsyncRowset<'_>> {
        self.connection.execute_query(query).await
    }

    /// Fetches a single row, erroring if zero rows are returned.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::fetch_one`].
    pub async fn fetch_one<Q: AsRef<str>>(&self, query: Q) -> Result<Row> {
        self.connection.fetch_one(query).await
    }

    /// Fetches a single row or `None`.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::fetch_optional`].
    pub async fn fetch_optional<Q: AsRef<str>>(&self, query: Q) -> Result<Option<Row>> {
        self.connection.fetch_optional(query).await
    }

    /// Fetches all rows from the query.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::fetch_all`].
    pub async fn fetch_all<Q: AsRef<str>>(&self, query: Q) -> Result<Vec<Row>> {
        self.connection.fetch_all(query).await
    }

    /// Fetches a single non-NULL scalar; errors if empty or NULL.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::fetch_scalar`].
    pub async fn fetch_scalar<T, Q>(&self, query: Q) -> Result<T>
    where
        T: RowValue,
        Q: AsRef<str>,
    {
        self.connection.fetch_scalar(query).await
    }

    /// Fetches a single scalar, allowing NULL as `None`.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::fetch_optional_scalar`].
    pub async fn fetch_optional_scalar<T, Q>(&self, query: Q) -> Result<Option<T>>
    where
        T: RowValue,
        Q: AsRef<str>,
    {
        self.connection.fetch_optional_scalar(query).await
    }

    /// Returns a count from a `SELECT COUNT(*)` style query.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::query_count`].
    pub async fn query_count(&self, query: &str) -> Result<i64> {
        self.connection.query_count(query).await
    }

    /// Executes a parameterized query within this transaction.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::query_params`].
    pub async fn query_params(
        &self,
        query: &str,
        params: &[&dyn crate::params::ToSqlParam],
    ) -> Result<AsyncRowset<'_>> {
        self.connection.query_params(query, params).await
    }

    /// Executes a parameterized command within this transaction.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`AsyncConnection::command_params`].
    pub async fn command_params(
        &self,
        query: &str,
        params: &[&dyn crate::params::ToSqlParam],
    ) -> Result<u64> {
        self.connection.command_params(query, params).await
    }
}

impl Drop for AsyncTransaction<'_> {
    fn drop(&mut self) {
        if !self.completed {
            // CRITICAL: Rust does not support async Drop, so we CANNOT issue a
            // ROLLBACK here (unlike the sync Transaction which can and does).
            // The transaction remains open on the server until the next command
            // on the connection, at which point Hyper handles it implicitly.
            // Users MUST explicitly call commit() or rollback().
            tracing::warn!(
                "AsyncTransaction dropped without explicit commit/rollback — \
                 transaction state is undefined until the next command on this connection"
            );
        }
    }
}
