// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async connection pool for Hyper database.
//!
//! This module provides connection pooling via [`deadpool`] for efficient
//! connection reuse in async applications.
//!
//! # Example
//!
//! ```no_run
//! use hyperdb_api::pool::{create_pool, PoolConfig};
//! use hyperdb_api::CreateMode;
//!
//! #[tokio::main]
//! async fn main() -> hyperdb_api::Result<()> {
//!     // Create a pool configuration
//!     let config = PoolConfig::new("localhost:7483", "example.hyper")
//!         .create_mode(CreateMode::CreateIfNotExists)
//!         .max_size(16);
//!
//!     // Build the pool
//!     let pool = create_pool(config)?;
//!
//!     // Get a connection from the pool
//!     let conn = pool.get().await.map_err(|e| hyperdb_api::Error::new(e.to_string()))?;
//!
//!     // Use the connection
//!     conn.execute_command("SELECT 1").await?;
//!
//!     // Connection is returned to pool when dropped
//!     Ok(())
//! }
//! ```
//!
//! # Lifecycle hooks
//!
//! `PoolConfig` supports two async lifecycle hooks for users who need to
//! customize per-connection or per-checkout behavior:
//!
//! - `after_connect` runs once on every newly-opened connection (useful for
//!   `SET search_path`, prepared-statement warmup, etc.)
//! - `before_acquire` runs every time a connection is checked out (useful
//!   for session reset, telemetry, custom health checks)
//!
//! `health_check(bool)` toggles the default per-checkout `SELECT 1` probe —
//! disable it on hot paths where the roundtrip cost outweighs the value of
//! catching a half-dead connection at acquire time.
//!
//! ```no_run
//! use hyperdb_api::pool::{create_pool, PoolConfig};
//! use hyperdb_api::CreateMode;
//!
//! # #[tokio::main]
//! # async fn main() -> hyperdb_api::Result<()> {
//! let config = PoolConfig::new("localhost:7483", "example.hyper")
//!     .create_mode(CreateMode::CreateIfNotExists)
//!     .max_size(16)
//!     .health_check(false) // skip per-checkout SELECT 1
//!     .after_connect(|conn| Box::pin(async move {
//!         conn.execute_command("SET search_path TO public").await?;
//!         Ok(())
//!     }));
//! let _pool = create_pool(config)?;
//! # Ok(())
//! # }
//! ```

use std::pin::Pin;
use std::sync::Arc;

use deadpool::managed::{self, Manager, Metrics, RecycleError, RecycleResult};
use tokio::sync::Mutex as AsyncMutex;

use crate::async_connection::AsyncConnection;
use crate::error::{Error, Result};
use crate::CreateMode;

/// Future returned by pool lifecycle hooks.
pub type HookFuture<'a> = Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

/// A hook that runs once on every newly-opened connection (after authentication
/// and any database-creation handshake). Use it to set session variables, install
/// statement caches, warm prepared statements, etc.
///
/// Returning `Err` from the hook causes pool creation to fail and the connection
/// to be dropped.
pub type AfterConnectHook = Arc<dyn Fn(&AsyncConnection) -> HookFuture<'_> + Send + Sync + 'static>;

/// A hook that runs every time a connection is checked out of the pool, before
/// it is handed to the caller. Use it for per-acquire health checks, session
/// resets, or telemetry.
///
/// Returning `Err` from the hook causes the connection to be evicted (the pool
/// retries with another connection or builds a new one).
pub type BeforeAcquireHook =
    Arc<dyn Fn(&AsyncConnection) -> HookFuture<'_> + Send + Sync + 'static>;

/// Configuration for the connection pool.
#[derive(Clone)]
pub struct PoolConfig {
    /// Server endpoint (e.g., "localhost:7483" or "<http://localhost:7484>")
    pub endpoint: String,
    /// Database path
    pub database: String,
    /// Database creation mode (only used for first connection)
    pub create_mode: CreateMode,
    /// Optional username for authentication
    pub user: Option<String>,
    /// Optional password for authentication
    pub password: Option<String>,
    /// Maximum number of connections in the pool
    pub max_size: usize,
    /// If `false`, skip the per-checkout `SELECT 1` health probe in `recycle()`.
    /// Defaults to `true`. Disable for hot paths where the roundtrip cost matters
    /// more than detecting a half-dead connection at acquire time. The pool
    /// still reaps connections via [`AsyncConnection::is_alive`].
    pub health_check: bool,
    /// Optional hook run on every newly-opened connection (see [`AfterConnectHook`]).
    pub after_connect: Option<AfterConnectHook>,
    /// Optional hook run on every checkout (see [`BeforeAcquireHook`]).
    pub before_acquire: Option<BeforeAcquireHook>,
}

impl std::fmt::Debug for PoolConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolConfig")
            .field("endpoint", &self.endpoint)
            .field("database", &self.database)
            .field("create_mode", &self.create_mode)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("max_size", &self.max_size)
            .field("health_check", &self.health_check)
            .field(
                "after_connect",
                &self.after_connect.as_ref().map(|_| "<fn>"),
            )
            .field(
                "before_acquire",
                &self.before_acquire.as_ref().map(|_| "<fn>"),
            )
            .finish()
    }
}

impl PoolConfig {
    /// Creates a new pool configuration.
    pub fn new(endpoint: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            database: database.into(),
            create_mode: CreateMode::DoNotCreate,
            user: None,
            password: None,
            max_size: 16,
            health_check: true,
            after_connect: None,
            before_acquire: None,
        }
    }

    /// Sets the database creation mode.
    #[must_use]
    pub fn create_mode(mut self, mode: CreateMode) -> Self {
        self.create_mode = mode;
        self
    }

    #[must_use]
    /// Sets authentication credentials.
    pub fn auth(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self.password = Some(password.into());
        self
    }

    /// Sets the maximum pool size.
    #[must_use]
    pub fn max_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    /// Enables or disables the per-checkout `SELECT 1` health probe.
    /// Defaults to enabled. Disable on hot paths where the roundtrip cost
    /// outweighs the value of catching a dead connection at acquire time.
    #[must_use]
    pub fn health_check(mut self, enabled: bool) -> Self {
        self.health_check = enabled;
        self
    }

    /// Installs a hook that runs on every newly-opened connection.
    ///
    /// Use this to apply session-level setup (e.g. `SET search_path`, install
    /// prepared statements). The hook is called once per physical connection,
    /// not per checkout.
    #[must_use]
    pub fn after_connect<F>(mut self, hook: F) -> Self
    where
        F: Fn(&AsyncConnection) -> HookFuture<'_> + Send + Sync + 'static,
    {
        self.after_connect = Some(Arc::new(hook));
        self
    }

    /// Installs a hook that runs on every connection checkout, before the
    /// connection is handed to the caller.
    ///
    /// Returning `Err` from the hook evicts the connection from the pool;
    /// the caller's `pool.get()` then retries with another connection or
    /// builds a new one. Use this for per-acquire health checks beyond the
    /// default `SELECT 1` probe (e.g. validating session state).
    #[must_use]
    pub fn before_acquire<F>(mut self, hook: F) -> Self
    where
        F: Fn(&AsyncConnection) -> HookFuture<'_> + Send + Sync + 'static,
    {
        self.before_acquire = Some(Arc::new(hook));
        self
    }
}

/// Connection pool manager for `AsyncConnection`.
///
/// The first call to [`Manager::create`] holds an async mutex while attempting
/// to open a connection with the configured [`CreateMode`]. Concurrent callers
/// wait for that attempt to finish, then use `CreateMode::DoNotCreate`. If the
/// first attempt fails, the next caller retries with the original create_mode
/// (for idempotent modes only — `Create` is not retried because a sibling
/// connection may have already created the database).
#[derive(Debug)]
pub struct ConnectionManager {
    config: Arc<PoolConfig>,
    /// Synchronizes the first-connection attempt across concurrent callers.
    /// `Some(())` after the first successful attempt; held while a first
    /// attempt is in progress to serialize concurrent races. The value is the
    /// outcome of the first call (the database is now known to exist).
    init_lock: Arc<AsyncMutex<bool>>,
}

impl ConnectionManager {
    /// Creates a new connection manager.
    #[must_use]
    pub fn new(config: PoolConfig) -> Self {
        Self {
            config: Arc::new(config),
            init_lock: Arc::new(AsyncMutex::new(false)),
        }
    }

    async fn open(&self, mode: CreateMode) -> Result<AsyncConnection> {
        if let (Some(user), Some(password)) = (&self.config.user, &self.config.password) {
            AsyncConnection::connect_with_auth(
                &self.config.endpoint,
                &self.config.database,
                mode,
                user,
                password,
            )
            .await
        } else {
            AsyncConnection::connect(&self.config.endpoint, &self.config.database, mode).await
        }
    }
}

impl Manager for ConnectionManager {
    type Type = AsyncConnection;
    type Error = Error;

    async fn create(&self) -> Result<AsyncConnection> {
        // Fast path: if the first connection already succeeded, just open with
        // DoNotCreate. We hold the lock briefly to read the flag.
        // (Lock is uncontended after the first connection — fast path is cheap.)
        let conn = {
            let initialized = self.init_lock.lock().await;
            if *initialized {
                drop(initialized);
                self.open(CreateMode::DoNotCreate).await?
            } else {
                drop(initialized);
                // Slow path: first creation. Acquire the lock and re-check (in
                // case another waiter raced us), then attempt with the
                // configured mode.
                let mut initialized = self.init_lock.lock().await;
                if *initialized {
                    drop(initialized);
                    self.open(CreateMode::DoNotCreate).await?
                } else {
                    let result = self.open(self.config.create_mode).await;
                    if result.is_ok() {
                        *initialized = true;
                    }
                    // On failure leave `initialized = false` so the next caller
                    // retries with the original create_mode.
                    result?
                }
            }
        };

        // Run the after_connect hook (if any) before handing the connection
        // to the pool. Hook errors propagate as connection-creation errors.
        if let Some(hook) = self.config.after_connect.as_ref() {
            hook(&conn).await?;
        }
        Ok(conn)
    }

    async fn recycle(
        &self,
        conn: &mut AsyncConnection,
        _metrics: &Metrics,
    ) -> RecycleResult<Self::Error> {
        // Optional `SELECT 1` health probe. Off by default if the user
        // disables it via PoolConfig::health_check(false).
        if self.config.health_check {
            conn.execute_command("SELECT 1")
                .await
                .map_err(RecycleError::Backend)?;
        }
        // Per-checkout user hook (e.g. session reset, telemetry).
        if let Some(hook) = self.config.before_acquire.as_ref() {
            hook(conn).await.map_err(RecycleError::Backend)?;
        }
        Ok(())
    }
}

/// A pool of async connections to a Hyper database.
///
/// This pool manages a set of reusable connections, automatically creating
/// new connections when needed and recycling them after use.
pub type Pool = managed::Pool<ConnectionManager>;

/// A pooled connection wrapper.
pub type PooledConnection = managed::Object<ConnectionManager>;

/// Creates a new connection pool from configuration.
///
/// # Errors
///
/// Returns [`Error::Other`] wrapping the `deadpool` builder failure if
/// the pool cannot be constructed (e.g. invalid `max_size`). Connections
/// themselves are opened lazily on first use, so endpoint/auth errors
/// surface from [`Pool::get`](managed::Pool::get), not here.
pub fn create_pool(config: PoolConfig) -> Result<Pool> {
    let max_size = config.max_size;
    let manager = ConnectionManager::new(config);
    Pool::builder(manager)
        .max_size(max_size)
        .build()
        .map_err(|e| Error::new(format!("Failed to create pool: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_config_builder() {
        let config = PoolConfig::new("localhost:7483", "test.hyper")
            .create_mode(CreateMode::CreateIfNotExists)
            .auth("user", "pass")
            .max_size(32);

        assert_eq!(config.endpoint, "localhost:7483");
        assert_eq!(config.database, "test.hyper");
        assert_eq!(config.create_mode, CreateMode::CreateIfNotExists);
        assert_eq!(config.user, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
        assert_eq!(config.max_size, 32);
    }
}
