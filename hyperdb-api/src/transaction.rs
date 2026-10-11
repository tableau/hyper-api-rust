// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! RAII transaction guard for synchronous connections.

use crate::connection::{Connection, ScalarValue};
use crate::error::Result;

/// An RAII transaction guard that automatically rolls back on drop if not committed.
///
/// Created via [`Connection::transaction()`]. This **exclusively borrows** the
/// connection for the lifetime of the transaction, preventing any other code from
/// using the raw connection while the transaction is active. This is enforced at
/// compile time by Rust's borrow checker.
///
/// # Example
///
/// ```no_run
/// # use hyperdb_api::{Connection, CreateMode, Result};
/// # fn main() -> Result<()> {
/// # let mut conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::DoNotCreate)?;
/// // Transaction auto-rolls back if commit() is not called
/// let txn = conn.transaction()?;
/// txn.execute_command("INSERT INTO users VALUES (1, 'Alice')")?;
/// txn.execute_command("INSERT INTO users VALUES (2, 'Bob')")?;
/// txn.commit()?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Transaction<'conn> {
    connection: &'conn mut Connection,
    completed: bool,
}

impl<'conn> Transaction<'conn> {
    /// Creates a new transaction by issuing `BEGIN TRANSACTION`.
    pub(crate) fn new(connection: &'conn mut Connection) -> Result<Self> {
        // Delegate to the public `*_unguarded` primitive; the guard adds the
        // pairing guarantee (commit/rollback consume `self`, drop rolls back).
        connection.begin_transaction_unguarded()?;
        Ok(Self {
            connection,
            completed: false,
        })
    }

    /// Commits the transaction.
    ///
    /// # Errors
    ///
    /// Returns the error from the server's `COMMIT`. The transaction is
    /// marked completed regardless, so the drop guard will not re-issue a
    /// rollback.
    pub fn commit(mut self) -> Result<()> {
        self.completed = true;
        self.connection.commit_unguarded()
    }

    /// Rolls back the transaction explicitly.
    ///
    /// # Errors
    ///
    /// Returns the error from the server's `ROLLBACK`. The transaction
    /// is marked completed regardless.
    pub fn rollback(mut self) -> Result<()> {
        self.completed = true;
        self.connection.rollback_unguarded()
    }

    /// Returns a reference to the underlying connection.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        self.connection
    }

    // =========================================================================
    // Delegated execution methods
    // =========================================================================

    /// Executes a SQL command within this transaction.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::execute_command`].
    pub fn execute_command(&self, sql: &str) -> Result<u64> {
        self.connection.execute_command(sql)
    }

    /// Executes a query and returns streaming results within this transaction.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::execute_query`].
    pub fn execute_query(&self, query: &str) -> Result<crate::Rowset<'_>> {
        self.connection.execute_query(query)
    }

    /// Fetches a single row from a query.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::fetch_one`].
    pub fn fetch_one<Q: AsRef<str>>(&self, query: Q) -> Result<crate::Row> {
        self.connection.fetch_one(query)
    }

    /// Fetches an optional single row from a query.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::fetch_optional`].
    pub fn fetch_optional<Q: AsRef<str>>(&self, query: Q) -> Result<Option<crate::Row>> {
        self.connection.fetch_optional(query)
    }

    /// Fetches all rows from a query.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::fetch_all`].
    pub fn fetch_all<Q: AsRef<str>>(&self, query: Q) -> Result<Vec<crate::Row>> {
        self.connection.fetch_all(query)
    }

    /// Fetches a single scalar value from a query.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::fetch_scalar`].
    pub fn fetch_scalar<T, Q>(&self, query: Q) -> Result<T>
    where
        T: ScalarValue + crate::result::RowValue,
        Q: AsRef<str>,
    {
        self.connection.fetch_scalar(query)
    }

    /// Fetches an optional scalar value from a query.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::fetch_optional_scalar`].
    pub fn fetch_optional_scalar<T, Q>(&self, query: Q) -> Result<Option<T>>
    where
        T: ScalarValue + crate::result::RowValue,
        Q: AsRef<str>,
    {
        self.connection.fetch_optional_scalar(query)
    }

    /// Queries for a count value, defaulting to 0 if NULL.
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::query_count`].
    pub fn query_count(&self, query: &str) -> Result<i64> {
        self.connection.query_count(query)
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.completed {
            // Best-effort rollback: a `Drop` cannot report failure, so log
            // instead of swallowing silently. This is the only trace of an
            // implicit rollback — on the panic path there is no `Err` for the
            // caller to inspect.
            // Hyper produces a WARNING (not error) if no active transaction.
            if let Err(e) = self.connection.rollback_unguarded() {
                tracing::warn!(
                    error = %e,
                    "Transaction dropped without commit/rollback and the implicit ROLLBACK failed \
                     — the transaction may still be open on this connection"
                );
            } else {
                tracing::debug!(
                    "Transaction dropped without commit/rollback — issued implicit ROLLBACK"
                );
            }
        }
    }
}
