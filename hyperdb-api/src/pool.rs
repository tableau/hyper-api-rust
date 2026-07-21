// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Connection pools for Hyper database.
//!
//! This module provides two pools that share a common configuration surface:
//!
//! - [`Pool`] — an async pool built on [`deadpool`], for `async`/`await`
//!   applications. Created via [`create_pool`].
//! - [`ConnectionPool`] — a synchronous, r2d2-style pool with **no Tokio
//!   dependency** on its hot path, for blocking applications. Created via
//!   [`SyncPoolConfig::build`].
//!
//! Both pools open connections lazily, serialize the first-connection
//! database-creation handshake, recycle connections via a configurable
//! [`RecycleStrategy`] / [`SyncRecycleStrategy`], and enforce optional
//! lifetime / idle caps.
//!
//! # Async example
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
//!     let conn = pool.get().await.map_err(|e| hyperdb_api::Error::internal(e.to_string()))?;
//!
//!     // Use the connection
//!     conn.execute_command("SELECT 1").await?;
//!
//!     // Connection is returned to pool when dropped
//!     Ok(())
//! }
//! ```
//!
//! # Sync example
//!
//! ```no_run
//! use hyperdb_api::pool::SyncPoolConfig;
//! use hyperdb_api::CreateMode;
//! use std::time::Duration;
//!
//! # fn main() -> hyperdb_api::Result<()> {
//! let pool = SyncPoolConfig::new("localhost:7483", "example.hyper")
//!     .create_mode(CreateMode::CreateIfNotExists)
//!     .max_size(8)
//!     .wait_timeout(Some(Duration::from_secs(5)))
//!     .build();
//!
//! let conn = pool.get()?;
//! conn.execute_command("SELECT 1")?;
//! // Connection returns to the pool when `conn` drops.
//! # Ok(())
//! # }
//! ```
//!
//! # Tuning knobs (shared by both pools)
//!
//! - **Recycle strategy** ([`PoolConfig::recycle`] / [`SyncPoolConfig::recycle`])
//!   controls the per-checkout health probe. Defaults to `SelectOne` (a
//!   `SELECT 1` round-trip). Use `Ping` for the connection's native ping,
//!   `None` to skip the probe on hot paths, or `Custom(..)` for a bespoke check.
//! - **`max_lifetime`** caps how long a physical connection may live before it
//!   is retired at checkout, regardless of health.
//! - **`idle_timeout`** retires connections that have sat idle too long (down to
//!   `min_idle`, which is kept warm).
//! - **Timeouts** (`wait_timeout`, `create_timeout`, `recycle_timeout`) bound how
//!   long an acquire may block. The async pool enforces all three via deadpool's
//!   Tokio runtime; the sync pool enforces `wait_timeout` natively (see
//!   [`SyncPoolConfig`] for the create/recycle caveat).
//!
//! # Lifecycle hooks (async pool only)
//!
//! `PoolConfig` supports two async lifecycle hooks:
//!
//! - `after_connect` runs once on every newly-opened connection (useful for
//!   `SET search_path`, prepared-statement warmup, etc.)
//! - `before_acquire` runs every time a connection is checked out (useful
//!   for session reset, telemetry, custom health checks)
//!
//! ```no_run
//! use hyperdb_api::pool::{create_pool, PoolConfig, RecycleStrategy};
//! use hyperdb_api::CreateMode;
//!
//! # #[tokio::main]
//! # async fn main() -> hyperdb_api::Result<()> {
//! let config = PoolConfig::new("localhost:7483", "example.hyper")
//!     .create_mode(CreateMode::CreateIfNotExists)
//!     .max_size(16)
//!     .recycle(RecycleStrategy::None) // skip the per-checkout probe
//!     .after_connect(|conn| Box::pin(async move {
//!         conn.execute_command("SET search_path TO public").await?;
//!         Ok(())
//!     }));
//! let _pool = create_pool(config)?;
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use deadpool::managed::{self, Manager, Metrics, RecycleError, RecycleResult, Timeouts};
use deadpool::Runtime;
use tokio::sync::Mutex as AsyncMutex;

use crate::async_connection::AsyncConnection;
use crate::connection::Connection;
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

/// A user-supplied async per-checkout health check for [`RecycleStrategy::Custom`].
///
/// Returning `Err` evicts the connection from the pool.
pub type RecycleCheck = Arc<dyn Fn(&AsyncConnection) -> HookFuture<'_> + Send + Sync + 'static>;

/// Strategy used by the async [`Pool`] to validate a connection when it is
/// checked back out (recycled).
///
/// Defaults to [`SelectOne`](RecycleStrategy::SelectOne). The probe runs on
/// every acquire after the connection's passive liveness check; a failing probe
/// evicts the connection and the pool transparently builds a fresh one.
#[derive(Clone, Default)]
pub enum RecycleStrategy {
    /// Run a `SELECT 1` round-trip on every checkout. The default — catches a
    /// half-dead connection at acquire time at the cost of one round-trip.
    #[default]
    SelectOne,
    /// Call [`AsyncConnection::ping`] on every checkout (equivalent round-trip,
    /// expressed via the connection's own health primitive).
    Ping,
    /// Skip the active probe entirely. The pool still drops connections that
    /// fail the passive [`AsyncConnection::is_alive`] check. Use on hot paths
    /// where the round-trip cost outweighs detecting a dead connection early.
    None,
    /// Run a user-supplied async check on every checkout.
    Custom(RecycleCheck),
}

impl std::fmt::Debug for RecycleStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SelectOne => f.write_str("SelectOne"),
            Self::Ping => f.write_str("Ping"),
            Self::None => f.write_str("None"),
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

/// Configuration for the async connection pool.
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
    /// If `false`, skip the per-checkout health probe. Retained for backwards
    /// compatibility — it is kept in sync with [`recycle`](Self::recycle) by the
    /// [`health_check`](Self::health_check) and [`recycle`](Self::recycle)
    /// builders. Prefer setting [`recycle`](PoolConfig::recycle) directly.
    pub health_check: bool,
    /// Strategy used to validate connections on checkout. Defaults to
    /// [`RecycleStrategy::SelectOne`].
    pub recycle: RecycleStrategy,
    /// Maximum time to wait for a slot to become available on
    /// [`get`](managed::Pool::get). `None` waits indefinitely (the default).
    pub wait_timeout: Option<Duration>,
    /// Maximum time to wait for a new connection to be created. `None` disables
    /// the cap (the default).
    pub create_timeout: Option<Duration>,
    /// Maximum time to wait for the recycle probe to complete. `None` disables
    /// the cap (the default).
    pub recycle_timeout: Option<Duration>,
    /// Maximum lifetime of a physical connection before it is retired at
    /// checkout, regardless of health. `None` disables the cap (the default).
    pub max_lifetime: Option<Duration>,
    /// Maximum time a connection may sit idle before it is retired at checkout.
    /// `None` disables the cap (the default).
    pub idle_timeout: Option<Duration>,
    /// Minimum number of idle connections to keep warm (not eagerly created;
    /// used to bias eviction decisions). `None` means no floor (the default).
    pub min_idle: Option<u32>,
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
            .field("recycle", &self.recycle)
            .field("wait_timeout", &self.wait_timeout)
            .field("create_timeout", &self.create_timeout)
            .field("recycle_timeout", &self.recycle_timeout)
            .field("max_lifetime", &self.max_lifetime)
            .field("idle_timeout", &self.idle_timeout)
            .field("min_idle", &self.min_idle)
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
    ///
    /// All optional knobs default to `None`/disabled and `recycle` defaults to
    /// [`RecycleStrategy::SelectOne`], so a config that sets only
    /// endpoint/database/`max_size` behaves identically to prior versions.
    pub fn new(endpoint: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            database: database.into(),
            create_mode: CreateMode::DoNotCreate,
            user: None,
            password: None,
            max_size: 16,
            health_check: true,
            recycle: RecycleStrategy::SelectOne,
            wait_timeout: None,
            create_timeout: None,
            recycle_timeout: None,
            max_lifetime: None,
            idle_timeout: None,
            min_idle: None,
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

    /// Enables or disables the per-checkout health probe.
    ///
    /// Backwards-compatible shorthand for [`recycle`](Self::recycle): `true`
    /// selects [`RecycleStrategy::SelectOne`], `false` selects
    /// [`RecycleStrategy::None`]. Prefer `recycle` for finer control.
    #[must_use]
    pub fn health_check(mut self, enabled: bool) -> Self {
        self.health_check = enabled;
        self.recycle = if enabled {
            RecycleStrategy::SelectOne
        } else {
            RecycleStrategy::None
        };
        self
    }

    /// Sets the per-checkout recycle strategy. Keeps the legacy
    /// [`health_check`](Self::health_check) flag in sync (`false` iff the
    /// strategy is [`RecycleStrategy::None`]).
    #[must_use]
    pub fn recycle(mut self, strategy: RecycleStrategy) -> Self {
        self.health_check = !matches!(strategy, RecycleStrategy::None);
        self.recycle = strategy;
        self
    }

    /// Sets the maximum time to wait for an available slot on `get`.
    #[must_use]
    pub fn wait_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.wait_timeout = timeout;
        self
    }

    /// Sets the maximum time to wait for a new connection to be created.
    #[must_use]
    pub fn create_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.create_timeout = timeout;
        self
    }

    /// Sets the maximum time to wait for the recycle probe to complete.
    #[must_use]
    pub fn recycle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.recycle_timeout = timeout;
        self
    }

    /// Sets the maximum lifetime of a physical connection.
    #[must_use]
    pub fn max_lifetime(mut self, lifetime: Option<Duration>) -> Self {
        self.max_lifetime = lifetime;
        self
    }

    /// Sets the maximum idle time before a connection is retired at checkout.
    #[must_use]
    pub fn idle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Sets the minimum number of idle connections to keep warm.
    #[must_use]
    pub fn min_idle(mut self, min_idle: Option<u32>) -> Self {
        self.min_idle = min_idle;
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
    /// configured [`recycle`](Self::recycle) probe (e.g. validating session state).
    #[must_use]
    pub fn before_acquire<F>(mut self, hook: F) -> Self
    where
        F: Fn(&AsyncConnection) -> HookFuture<'_> + Send + Sync + 'static,
    {
        self.before_acquire = Some(Arc::new(hook));
        self
    }

    /// Returns `true` if any deadpool-enforced timeout is configured (and thus a
    /// Tokio runtime must be wired into the builder).
    fn has_timeout(&self) -> bool {
        self.wait_timeout.is_some()
            || self.create_timeout.is_some()
            || self.recycle_timeout.is_some()
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
        metrics: &Metrics,
    ) -> RecycleResult<Self::Error> {
        // Retire connections that have outlived their configured caps before
        // spending a round-trip probing them. Returning a `Message` error evicts
        // the connection; deadpool then builds a fresh one transparently.
        if let Some(max_lifetime) = self.config.max_lifetime {
            if metrics.age() >= max_lifetime {
                return Err(RecycleError::message("connection exceeded max_lifetime"));
            }
        }
        if let Some(idle_timeout) = self.config.idle_timeout {
            if metrics.last_used() >= idle_timeout {
                return Err(RecycleError::message("connection exceeded idle_timeout"));
            }
        }

        // Active health probe per the configured strategy.
        match &self.config.recycle {
            RecycleStrategy::SelectOne => {
                conn.execute_command("SELECT 1")
                    .await
                    .map_err(RecycleError::Backend)?;
            }
            RecycleStrategy::Ping => {
                conn.ping().await.map_err(RecycleError::Backend)?;
            }
            RecycleStrategy::None => {}
            RecycleStrategy::Custom(check) => {
                check(conn).await.map_err(RecycleError::Backend)?;
            }
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
/// Returns [`Error::Config`] wrapping the `deadpool` builder failure if
/// the pool cannot be constructed (e.g. invalid `max_size`). Connections
/// themselves are opened lazily on first use, so endpoint/auth errors
/// surface from [`Pool::get`](managed::Pool::get), not here.
pub fn create_pool(config: PoolConfig) -> Result<Pool> {
    let max_size = config.max_size;
    let timeouts = Timeouts {
        wait: config.wait_timeout,
        create: config.create_timeout,
        recycle: config.recycle_timeout,
    };
    // deadpool requires a runtime to enforce any timeout; only wire one in when
    // a timeout is actually configured so the zero-config path stays untouched.
    let needs_runtime = config.has_timeout();
    let manager = ConnectionManager::new(config);
    let mut builder = Pool::builder(manager).max_size(max_size).timeouts(timeouts);
    if needs_runtime {
        builder = builder.runtime(Runtime::Tokio1);
    }
    builder
        .build()
        .map_err(|e| Error::config(format!("Failed to create pool: {e}")))
}

// ---------------------------------------------------------------------------
// Synchronous, r2d2-style pool (no Tokio on the hot path).
// ---------------------------------------------------------------------------

/// A user-supplied synchronous per-checkout health check for
/// [`SyncRecycleStrategy::Custom`].
///
/// Returning `Err` evicts the connection from the pool.
pub type SyncRecycleCheck = Arc<dyn Fn(&Connection) -> Result<()> + Send + Sync + 'static>;

/// Strategy used by the synchronous [`ConnectionPool`] to validate a connection
/// when it is checked out.
///
/// Mirrors [`RecycleStrategy`] for the blocking [`Connection`] type. Defaults to
/// [`SelectOne`](SyncRecycleStrategy::SelectOne).
#[derive(Clone, Default)]
pub enum SyncRecycleStrategy {
    /// Run a `SELECT 1` round-trip on every checkout (the default).
    #[default]
    SelectOne,
    /// Call [`Connection::ping`] on every checkout.
    Ping,
    /// Skip the active probe; only the passive [`Connection::is_alive`] check runs.
    None,
    /// Run a user-supplied synchronous check on every checkout.
    Custom(SyncRecycleCheck),
}

impl std::fmt::Debug for SyncRecycleStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SelectOne => f.write_str("SelectOne"),
            Self::Ping => f.write_str("Ping"),
            Self::None => f.write_str("None"),
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

/// Configuration and builder for the synchronous [`ConnectionPool`].
///
/// Mirrors the async [`PoolConfig`] surface. All optional knobs default to
/// `None`/disabled and `recycle` defaults to
/// [`SyncRecycleStrategy::SelectOne`], so a config that sets only
/// endpoint/database/`max_size` opens connections lazily and probes them with
/// `SELECT 1` on checkout — the natural baseline.
///
/// # Timeout support
///
/// The sync pool enforces [`wait_timeout`](Self::wait_timeout) natively (it
/// bounds how long [`ConnectionPool::get`] blocks waiting for a slot).
/// `create_timeout` and `recycle_timeout` require an async runtime to interrupt
/// a blocking syscall and are therefore **async-only** ([`PoolConfig`]); they
/// are intentionally absent here to avoid pulling Tokio into the sync path.
#[derive(Clone)]
pub struct SyncPoolConfig {
    /// Server endpoint (e.g., "localhost:7483").
    pub endpoint: String,
    /// Database path.
    pub database: String,
    /// Database creation mode (only used for the first connection).
    pub create_mode: CreateMode,
    /// Optional username for authentication.
    pub user: Option<String>,
    /// Optional password for authentication.
    pub password: Option<String>,
    /// Maximum number of connections in the pool.
    pub max_size: usize,
    /// Per-checkout recycle strategy. Defaults to [`SyncRecycleStrategy::SelectOne`].
    pub recycle: SyncRecycleStrategy,
    /// Maximum time [`ConnectionPool::get`] blocks waiting for a slot. `None`
    /// waits indefinitely (the default).
    pub wait_timeout: Option<Duration>,
    /// Maximum lifetime of a physical connection before it is retired at checkout.
    pub max_lifetime: Option<Duration>,
    /// Maximum idle time before a connection is retired at checkout (down to
    /// [`min_idle`](Self::min_idle), which is kept warm).
    pub idle_timeout: Option<Duration>,
    /// Minimum number of idle connections to keep warm (biases idle eviction).
    pub min_idle: Option<u32>,
}

impl std::fmt::Debug for SyncPoolConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncPoolConfig")
            .field("endpoint", &self.endpoint)
            .field("database", &self.database)
            .field("create_mode", &self.create_mode)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("max_size", &self.max_size)
            .field("recycle", &self.recycle)
            .field("wait_timeout", &self.wait_timeout)
            .field("max_lifetime", &self.max_lifetime)
            .field("idle_timeout", &self.idle_timeout)
            .field("min_idle", &self.min_idle)
            .finish()
    }
}

impl SyncPoolConfig {
    /// Creates a new synchronous pool configuration.
    pub fn new(endpoint: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            database: database.into(),
            create_mode: CreateMode::DoNotCreate,
            user: None,
            password: None,
            max_size: 16,
            recycle: SyncRecycleStrategy::SelectOne,
            wait_timeout: None,
            max_lifetime: None,
            idle_timeout: None,
            min_idle: None,
        }
    }

    /// Sets the database creation mode.
    #[must_use]
    pub fn create_mode(mut self, mode: CreateMode) -> Self {
        self.create_mode = mode;
        self
    }

    /// Sets authentication credentials.
    #[must_use]
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

    /// Sets the per-checkout recycle strategy.
    #[must_use]
    pub fn recycle(mut self, strategy: SyncRecycleStrategy) -> Self {
        self.recycle = strategy;
        self
    }

    /// Sets the maximum time `get` blocks waiting for a slot.
    #[must_use]
    pub fn wait_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.wait_timeout = timeout;
        self
    }

    /// Sets the maximum lifetime of a physical connection.
    #[must_use]
    pub fn max_lifetime(mut self, lifetime: Option<Duration>) -> Self {
        self.max_lifetime = lifetime;
        self
    }

    /// Sets the maximum idle time before a connection is retired at checkout.
    #[must_use]
    pub fn idle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Sets the minimum number of idle connections to keep warm.
    #[must_use]
    pub fn min_idle(mut self, min_idle: Option<u32>) -> Self {
        self.min_idle = min_idle;
        self
    }

    /// Builds the synchronous connection pool. Connections are opened lazily on
    /// first [`ConnectionPool::get`].
    #[must_use]
    pub fn build(self) -> ConnectionPool {
        ConnectionPool {
            inner: Arc::new(SyncPoolInner {
                config: self,
                state: Mutex::new(SyncPoolState {
                    idle: VecDeque::new(),
                    size: 0,
                    initialized: false,
                    init_in_progress: false,
                }),
                available: Condvar::new(),
            }),
        }
    }
}

/// An idle connection together with the bookkeeping needed to enforce
/// lifetime/idle caps.
struct IdleConn {
    conn: Connection,
    created: Instant,
    last_used: Instant,
}

/// Mutable pool state guarded by the pool mutex.
struct SyncPoolState {
    /// Idle connections available for checkout (LIFO via `pop_back`/`push_back`
    /// keeps the hottest connection warm).
    idle: VecDeque<IdleConn>,
    /// Total live connections (idle + checked out).
    size: usize,
    /// Set once the first-connection database-creation handshake has succeeded.
    initialized: bool,
    /// Held while a first-connection attempt is in flight, to serialize the
    /// create-mode handshake across threads (mirrors the async `init_lock`).
    init_in_progress: bool,
}

struct SyncPoolInner {
    config: SyncPoolConfig,
    state: Mutex<SyncPoolState>,
    available: Condvar,
}

impl SyncPoolInner {
    /// Opens a fresh physical connection, using the configured create mode only
    /// for the first connection.
    fn open(&self, first: bool) -> Result<Connection> {
        let mode = if first {
            self.config.create_mode
        } else {
            CreateMode::DoNotCreate
        };
        if let (Some(user), Some(password)) = (&self.config.user, &self.config.password) {
            Connection::connect_with_auth(
                &self.config.endpoint,
                &self.config.database,
                mode,
                user,
                password,
            )
        } else {
            Connection::connect(&self.config.endpoint, &self.config.database, mode)
        }
    }

    /// Returns `true` if an idle connection should be retired before reuse.
    ///
    /// `max_lifetime` is a hard cap. `idle_timeout` is honored only while doing
    /// so keeps the live count at or above `min_idle` (idle connections are kept
    /// warm down to that floor).
    fn should_evict(&self, idle: &IdleConn, live_size: usize) -> bool {
        if let Some(max_lifetime) = self.config.max_lifetime {
            if idle.created.elapsed() >= max_lifetime {
                return true;
            }
        }
        if let Some(idle_timeout) = self.config.idle_timeout {
            let min_idle = self.config.min_idle.unwrap_or(0) as usize;
            if idle.last_used.elapsed() >= idle_timeout && live_size > min_idle {
                return true;
            }
        }
        false
    }

    /// Runs the configured recycle probe against a connection.
    fn recycle(&self, conn: &Connection) -> Result<()> {
        if !conn.is_alive() {
            return Err(Error::connection("pooled connection is no longer alive"));
        }
        match &self.config.recycle {
            SyncRecycleStrategy::SelectOne => {
                conn.execute_command("SELECT 1")?;
            }
            SyncRecycleStrategy::Ping => {
                conn.ping()?;
            }
            SyncRecycleStrategy::None => {}
            SyncRecycleStrategy::Custom(check) => {
                check(conn)?;
            }
        }
        Ok(())
    }

    /// Returns a connection to the idle set and wakes a waiter.
    fn checkin(&self, conn: Connection, created: Instant) {
        {
            let mut state = self.state.lock().expect("pool mutex poisoned");
            state.idle.push_back(IdleConn {
                conn,
                created,
                last_used: Instant::now(),
            });
        }
        self.available.notify_one();
    }

    /// Drops a checked-out connection that the caller chose not to return,
    /// freeing its slot.
    fn discard(&self) {
        {
            let mut state = self.state.lock().expect("pool mutex poisoned");
            state.size = state.size.saturating_sub(1);
        }
        self.available.notify_one();
    }
}

/// A synchronous, r2d2-style pool of blocking [`Connection`]s.
///
/// Cloneable handles share one underlying pool. Connections are opened lazily,
/// validated on checkout per the configured [`SyncRecycleStrategy`], and
/// returned to the pool when the [`SyncPooledConnection`] guard drops. The hot
/// path uses only `std` synchronization primitives — no Tokio.
#[derive(Clone)]
pub struct ConnectionPool {
    inner: Arc<SyncPoolInner>,
}

impl std::fmt::Debug for ConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionPool")
            .field("config", &self.inner.config)
            .field("status", &self.status())
            .finish()
    }
}

impl ConnectionPool {
    /// Acquires a connection, blocking up to the configured
    /// [`wait_timeout`](SyncPoolConfig::wait_timeout) for a slot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Timeout`] if no slot becomes available within
    /// `wait_timeout`, or the underlying connection error if opening a new
    /// connection fails.
    pub fn get(&self) -> Result<SyncPooledConnection> {
        self.get_timeout(self.inner.config.wait_timeout)
    }

    /// Acquires a connection, blocking up to `timeout` for a slot (overriding
    /// the configured default). `None` blocks indefinitely.
    ///
    /// # Errors
    ///
    /// See [`get`](Self::get).
    ///
    /// # Panics
    ///
    /// Panics if the internal pool mutex has been poisoned by a thread that
    /// panicked while holding it.
    pub fn get_timeout(&self, timeout: Option<Duration>) -> Result<SyncPooledConnection> {
        let deadline = timeout.map(|t| Instant::now() + t);

        loop {
            // Decide what to do under the lock, then release it before any
            // blocking network I/O (connect / recycle probe).
            enum Action {
                Reuse(IdleConn),
                Create { first: bool },
                Wait,
            }

            let action = {
                let mut state = self.inner.state.lock().expect("pool mutex poisoned");
                if let Some(idle) = state.idle.pop_back() {
                    Action::Reuse(idle)
                } else if !state.initialized {
                    // First connection must run the create-mode handshake, and
                    // only one thread may do so. Others wait until it lands.
                    if state.init_in_progress {
                        Action::Wait
                    } else {
                        state.init_in_progress = true;
                        state.size += 1;
                        Action::Create { first: true }
                    }
                } else if state.size < self.inner.config.max_size {
                    state.size += 1;
                    Action::Create { first: false }
                } else {
                    Action::Wait
                }
            };

            match action {
                Action::Reuse(idle) => {
                    let live_size = {
                        let state = self.inner.state.lock().expect("pool mutex poisoned");
                        state.size
                    };
                    if self.inner.should_evict(&idle, live_size) {
                        self.inner.discard();
                        continue;
                    }
                    if self.inner.recycle(&idle.conn).is_ok() {
                        return Ok(SyncPooledConnection {
                            pool: Arc::clone(&self.inner),
                            conn: Some(idle.conn),
                            created: idle.created,
                        });
                    }
                    // Probe failed: drop this connection and loop again to reuse
                    // another idle one or build a fresh one.
                    self.inner.discard();
                }
                Action::Create { first } => match self.inner.open(first) {
                    Ok(conn) => {
                        if first {
                            let mut state = self.inner.state.lock().expect("pool mutex poisoned");
                            state.initialized = true;
                            state.init_in_progress = false;
                            drop(state);
                            // A successful first connection unblocks every
                            // waiter that parked on `init_in_progress`.
                            self.inner.available.notify_all();
                        }
                        return Ok(SyncPooledConnection {
                            pool: Arc::clone(&self.inner),
                            conn: Some(conn),
                            created: Instant::now(),
                        });
                    }
                    Err(e) => {
                        {
                            let mut state = self.inner.state.lock().expect("pool mutex poisoned");
                            state.size = state.size.saturating_sub(1);
                            if first {
                                state.init_in_progress = false;
                            }
                        }
                        // Wake waiters so the next one can retry the handshake.
                        self.inner.available.notify_all();
                        return Err(e);
                    }
                },
                Action::Wait => {
                    let state = self.inner.state.lock().expect("pool mutex poisoned");
                    // Re-check before parking to avoid a lost wakeup.
                    if !state.idle.is_empty()
                        || (state.initialized && state.size < self.inner.config.max_size)
                        || !state.initialized && !state.init_in_progress
                    {
                        continue;
                    }
                    match deadline {
                        Some(dl) => {
                            let now = Instant::now();
                            if now >= dl {
                                return Err(Error::timeout(
                                    "timed out waiting for an available pool connection",
                                ));
                            }
                            let (_guard, res) = self
                                .inner
                                .available
                                .wait_timeout(state, dl - now)
                                .expect("pool mutex poisoned");
                            if res.timed_out() {
                                return Err(Error::timeout(
                                    "timed out waiting for an available pool connection",
                                ));
                            }
                        }
                        None => {
                            let _guard = self
                                .inner
                                .available
                                .wait(state)
                                .expect("pool mutex poisoned");
                        }
                    }
                }
            }
        }
    }

    /// Returns the current pool status.
    ///
    /// # Panics
    ///
    /// Panics if the internal pool mutex has been poisoned.
    #[must_use]
    pub fn status(&self) -> PoolStatus {
        let state = self.inner.state.lock().expect("pool mutex poisoned");
        PoolStatus {
            idle: state.idle.len(),
            size: state.size,
            max_size: self.inner.config.max_size,
        }
    }
}

/// A snapshot of [`ConnectionPool`] occupancy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStatus {
    /// Number of idle connections currently available.
    pub idle: usize,
    /// Total live connections (idle + checked out).
    pub size: usize,
    /// Configured maximum pool size.
    pub max_size: usize,
}

/// A connection checked out of a synchronous [`ConnectionPool`].
///
/// Derefs to [`Connection`]. Returns to the pool when dropped; if it is dropped
/// while no longer alive (or after [`take`](Self::take)), the slot is freed
/// instead so the pool can build a replacement.
pub struct SyncPooledConnection {
    pool: Arc<SyncPoolInner>,
    conn: Option<Connection>,
    created: Instant,
}

impl SyncPooledConnection {
    /// Removes the connection from the pool's management, taking ownership.
    ///
    /// The pool slot is freed; the returned connection will not be recycled.
    ///
    /// # Panics
    ///
    /// Panics if the connection has already been taken out of this guard
    /// (only reachable via internal misuse — `take` consumes `self`).
    #[must_use]
    pub fn take(mut self) -> Connection {
        let conn = self.conn.take().expect("connection already taken");
        self.pool.discard();
        conn
    }
}

impl std::ops::Deref for SyncPooledConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().expect("connection already taken")
    }
}

impl std::fmt::Debug for SyncPooledConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncPooledConnection")
            .field("checked_out", &self.conn.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for SyncPooledConnection {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Return healthy connections to the pool; drop dead ones (freeing
            // the slot) so a replacement can be built on next checkout.
            if conn.is_alive() {
                self.pool.checkin(conn, self.created);
            } else {
                drop(conn);
                self.pool.discard();
            }
        }
    }
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

    #[test]
    fn test_pool_config_defaults_are_additive() {
        // A config that sets only the required fields must keep every new knob
        // at its zero-impact default so existing behavior is preserved.
        let config = PoolConfig::new("localhost:7483", "test.hyper");
        assert!(config.health_check);
        assert!(matches!(config.recycle, RecycleStrategy::SelectOne));
        assert_eq!(config.wait_timeout, None);
        assert_eq!(config.create_timeout, None);
        assert_eq!(config.recycle_timeout, None);
        assert_eq!(config.max_lifetime, None);
        assert_eq!(config.idle_timeout, None);
        assert_eq!(config.min_idle, None);
        assert!(!config.has_timeout());
    }

    #[test]
    fn test_health_check_and_recycle_stay_in_sync() {
        let off = PoolConfig::new("e", "d").health_check(false);
        assert!(!off.health_check);
        assert!(matches!(off.recycle, RecycleStrategy::None));

        let on = PoolConfig::new("e", "d").health_check(true);
        assert!(on.health_check);
        assert!(matches!(on.recycle, RecycleStrategy::SelectOne));

        let via_recycle = PoolConfig::new("e", "d").recycle(RecycleStrategy::None);
        assert!(!via_recycle.health_check);

        let via_ping = PoolConfig::new("e", "d").recycle(RecycleStrategy::Ping);
        assert!(via_ping.health_check);
        assert!(matches!(via_ping.recycle, RecycleStrategy::Ping));
    }

    #[test]
    fn test_pool_config_timeout_builders() {
        let config = PoolConfig::new("e", "d")
            .wait_timeout(Some(Duration::from_secs(1)))
            .create_timeout(Some(Duration::from_secs(2)))
            .recycle_timeout(Some(Duration::from_secs(3)))
            .max_lifetime(Some(Duration::from_secs(60)))
            .idle_timeout(Some(Duration::from_secs(30)))
            .min_idle(Some(2));
        assert_eq!(config.wait_timeout, Some(Duration::from_secs(1)));
        assert_eq!(config.create_timeout, Some(Duration::from_secs(2)));
        assert_eq!(config.recycle_timeout, Some(Duration::from_secs(3)));
        assert_eq!(config.max_lifetime, Some(Duration::from_secs(60)));
        assert_eq!(config.idle_timeout, Some(Duration::from_secs(30)));
        assert_eq!(config.min_idle, Some(2));
        assert!(config.has_timeout());
    }

    #[test]
    fn test_sync_pool_config_builder_and_defaults() {
        let config = SyncPoolConfig::new("localhost:7483", "test.hyper");
        assert_eq!(config.max_size, 16);
        assert!(matches!(config.recycle, SyncRecycleStrategy::SelectOne));
        assert_eq!(config.wait_timeout, None);
        assert_eq!(config.max_lifetime, None);
        assert_eq!(config.idle_timeout, None);
        assert_eq!(config.min_idle, None);

        let password: String = {
            use rand::RngExt;
            rand::rng().random_range(0..1_000_000).to_string()
        };

        let tuned = SyncPoolConfig::new("e", "d")
            .create_mode(CreateMode::CreateIfNotExists)
            .auth("u", password)
            .max_size(4)
            .recycle(SyncRecycleStrategy::Ping)
            .wait_timeout(Some(Duration::from_millis(500)))
            .max_lifetime(Some(Duration::from_secs(10)))
            .idle_timeout(Some(Duration::from_secs(5)))
            .min_idle(Some(1));
        assert_eq!(tuned.max_size, 4);
        assert!(matches!(tuned.recycle, SyncRecycleStrategy::Ping));
        assert_eq!(tuned.user, Some("u".to_string()));
        assert_eq!(tuned.wait_timeout, Some(Duration::from_millis(500)));
    }

    #[test]
    fn test_debug_redacts_password() {
        let dbg = format!("{:?}", PoolConfig::new("e", "d").auth("u", "secret"));
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("secret"));

        let sync_dbg = format!("{:?}", SyncPoolConfig::new("e", "d").auth("u", "secret"));
        assert!(sync_dbg.contains("<redacted>"));
        assert!(!sync_dbg.contains("secret"));
    }
}
