// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Directory watcher for incremental ingest.
//!
//! Producers coordinate with the watcher via a simple sentinel-file protocol:
//!
//! 1. Producer atomically writes the data file (e.g. `batch-0001.csv`).
//!    Usually this means writing to `batch-0001.csv.tmp` first and renaming.
//! 2. Producer creates a zero-byte companion file `batch-0001.csv.ready`.
//! 3. The watcher detects the `.ready` file, appends the paired data file to
//!    the target table, and deletes **both** files on success.
//! 4. On failure, the watcher moves both files into a `failed/` subdirectory
//!    and writes a `<name>.error` JSON file with the error details. The
//!    failed files are not retried — manual intervention is expected.
//!
//! # Security: TOCTOU and atomic file operations
//!
//! There is an inherent TOCTOU (time-of-check-to-time-of-use) race between
//! detecting the `.ready` sentinel and opening the data file for ingest.
//! Producers **must** use atomic file operations to avoid this:
//!
//! - Write data to a temporary file (e.g. `batch.csv.tmp`), then **rename**
//!   it to the final name (`batch.csv`). Do not write directly to the target.
//! - Never replace a data file with a symlink between writing and creating
//!   the `.ready` sentinel — the watcher resolves symlinks via
//!   `canonicalize()`, but the window between existence check and open
//!   cannot be fully eliminated without kernel-level file descriptors.
//! - On shared filesystems, ensure the rename is atomic (same mount point).
//!
//! Only one table per watched directory is supported; ingest is always in
//! append mode. File extensions decide the ingest path: `.csv`/`.json` go
//! through the CSV ingest (JSON-lines not supported today), `.parquet`/`.pq`
//! through the Parquet ingest, and `.arrow`/`.ipc`/`.feather` through the
//! Arrow IPC ingest.
//!
//! # Concurrency model
//!
//! Each watcher runs as a tokio task and checks out connections from a
//! per-watcher [`hyperdb_api::pool::Pool`] of [`hyperdb_api::AsyncConnection`]s.
//! Up to `max_concurrent` ingests run in parallel; every file runs inside
//! its own `BEGIN / COMMIT` on its own pooled connection, so the engine's
//! primary sync connection (used by `query`, `execute`, `chart`, etc.) is
//! never contended or forced to wait on a slow file.
//!
//! `notify` delivers events through its own std mpsc channel; we forward
//! them into a `tokio::sync::mpsc` on a small helper thread so the tokio
//! consumer can `.recv().await` naturally.

use crate::engine::Engine;
use crate::error::{ErrorCode, McpError};
use crate::ingest::{
    detect_file_format, ingest_csv_file_async, ingest_json_file_async, InferredFileFormat,
    IngestOptions,
};
use crate::ingest_arrow::{ingest_arrow_ipc_file_async, ingest_parquet_file_async};
use crate::subscriptions::{uris_for_table_change, SubscriptionRegistry};
use hyperdb_api::pool::{create_pool, Pool, PoolConfig};
use hyperdb_api::CreateMode;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Suffix used for the sentinel ("ready") file. Not a leading dot — append
/// this to the full data file name (e.g. `orders.csv` → `orders.csv.ready`).
pub const READY_SUFFIX: &str = ".ready";

/// Build a watcher connection pool from the current engine. Pulled out
/// of [`start_watching`] so the recovery path (post hyperd restart)
/// can call it again to swap in a fresh pool.
fn build_watcher_pool(
    engine: &Arc<Mutex<Option<Engine>>>,
    concurrency: usize,
) -> Result<Arc<Pool>, McpError> {
    let guard = engine
        .lock()
        .map_err(|_| McpError::new(ErrorCode::InternalError, "Engine lock poisoned"))?;
    let eng = guard.as_ref().ok_or_else(|| {
        McpError::new(
            ErrorCode::InternalError,
            "Engine not initialized when watcher pool requested",
        )
    })?;
    let endpoint = eng.hyperd_endpoint()?;
    // Watcher pool connects to the engine's primary (ephemeral). The
    // user-supplied table on `watch_directory` always lives there
    // unless a future flag routes it to the persistent attachment.
    let workspace = eng.ephemeral_path().to_string_lossy().to_string();
    let cfg = PoolConfig::new(endpoint, workspace)
        .create_mode(CreateMode::DoNotCreate)
        .max_size(concurrency);
    Ok(Arc::new(create_pool(cfg).map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("Failed to build watcher pool: {e}"),
        )
    })?))
}

/// Replace the watcher's pool atomically with a freshly-built one.
/// Called from the ingest path when a connection-lost error suggests
/// the underlying hyperd has been replaced (daemon restart, manual
/// `hyperd kill`).
async fn rebuild_watcher_pool(
    pool_slot: &tokio::sync::RwLock<Arc<Pool>>,
    engine: &Arc<Mutex<Option<Engine>>>,
    concurrency: usize,
) -> Result<(), McpError> {
    let new_pool = build_watcher_pool(engine, concurrency)?;
    let mut guard = pool_slot.write().await;
    *guard = new_pool;
    Ok(())
}

/// Default ceiling on parallel ingests per watcher. Chosen conservatively —
/// each parallel ingest holds one open TCP connection to hyperd plus a
/// transaction, and most workloads have fewer than 4 incoming streams at a
/// time.
pub const DEFAULT_MAX_CONCURRENT: usize = 4;

/// Hard upper bound on `max_concurrent` to prevent a runaway `watch_directory`
/// call from exhausting hyperd connections.
pub const MAX_CONCURRENT_LIMIT: usize = 32;

/// Options for [`start_watching`]. Use the builder-free literal form —
/// every field has a sensible default.
#[derive(Debug, Clone, Default)]
pub struct WatchOptions {
    /// Maximum number of files ingested in parallel. `0` means use
    /// [`DEFAULT_MAX_CONCURRENT`]. Values above [`MAX_CONCURRENT_LIMIT`]
    /// are clamped to the limit.
    pub max_concurrent: usize,
}

impl WatchOptions {
    fn resolved_concurrency(&self) -> usize {
        let n = if self.max_concurrent == 0 {
            DEFAULT_MAX_CONCURRENT
        } else {
            self.max_concurrent
        };
        n.clamp(1, MAX_CONCURRENT_LIMIT)
    }
}

/// Running counters for a watcher. Updated in place as the background task
/// processes events.
#[derive(Debug, Default, Clone)]
pub struct WatcherStats {
    pub files_ingested: u64,
    pub files_failed: u64,
    pub last_event_at: Option<SystemTime>,
    pub last_error: Option<String>,
    /// Configured parallelism ceiling (resolved to an actual number).
    pub max_concurrent: u32,
    /// Ingest tasks currently running — gives operators a live-load signal.
    pub in_flight: u32,
}

impl WatcherStats {
    fn snapshot(&self) -> Self {
        self.clone()
    }
}

/// Owns the notify watcher, the async ingest task, and the connection pool
/// for one watched directory.
///
/// Dropping the handle stops the watcher cleanly: the `Option<Watcher>` is
/// taken first, which drops the sender end of the mpsc channel, which causes
/// the worker task's `recv()` to return `None`, which ends the loop. We then
/// abort the task just in case (the `JoinHandle` is dropped non-blockingly —
/// the task has no cancellation-point awaits after `recv()`'s loop ends so
/// it completes naturally).
#[derive(Debug)]
pub struct WatcherHandle {
    pub directory: PathBuf,
    pub table: String,
    pub stats: Arc<Mutex<WatcherStats>>,
    /// Live counter of in-flight ingest tasks. Decremented on task completion
    /// via an RAII guard.
    in_flight: Arc<AtomicU32>,
    watcher: Option<RecommendedWatcher>,
    /// Handle to the tokio task that consumes notify events. Aborted on drop.
    task: Option<JoinHandle<()>>,
    /// Forwarder thread that bridges the std-sync notify sender to the
    /// tokio mpsc channel. Joined on drop after the notify watcher is
    /// dropped (which closes the std sender and lets this thread exit).
    forwarder: Option<std::thread::JoinHandle<()>>,
    /// Per-watcher connection pool. Wrapped in a tokio `RwLock` so the
    /// recovery path can swap in a fresh pool after a hyperd restart
    /// without disturbing in-flight ingests on the old pool. Kept here
    /// so the pool is torn down (all connections close) when the handle
    /// is dropped.
    _pool: Arc<tokio::sync::RwLock<Arc<Pool>>>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        // Drop the notify watcher first so its std-mpsc sender goes away;
        // that closes the forwarder's `rx`, which drops the tokio sender,
        // which ends the async consumer's loop.
        self.watcher.take();
        if let Some(t) = self.forwarder.take() {
            let _ = t.join();
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Registry of all active watchers, keyed by canonicalized directory path.
#[derive(Debug)]
pub struct WatcherRegistry {
    pub(crate) watchers: Mutex<HashMap<PathBuf, WatcherHandle>>,
}

impl WatcherRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            watchers: Mutex::new(HashMap::new()),
        }
    }

    /// Number of currently registered watchers. Intended for tests and
    /// diagnostics.
    pub fn len(&self) -> usize {
        self.watchers.lock().map_or(0, |g| g.len())
    }

    /// True when there are no active watchers.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Render the current set of watchers as a JSON array for the `status` tool.
    pub fn to_json(&self) -> Value {
        let Ok(guard) = self.watchers.lock() else {
            return Value::Array(Vec::new());
        };
        let now = SystemTime::now();
        let items: Vec<Value> = guard
            .values()
            .map(|h| {
                let stats = h.stats.lock().map(|s| s.snapshot()).unwrap_or_default();
                let in_flight = h.in_flight.load(Ordering::Relaxed);
                let last_event_ms_ago = stats
                    .last_event_at
                    .and_then(|t| now.duration_since(t).ok())
                    // `Duration::as_millis` is `u128`; saturate to
                    // `u64::MAX` on the absurd-long-duration edge
                    // instead of silently wrapping (AGENTS.md §9).
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
                json!({
                    "directory": h.directory.to_string_lossy(),
                    "table": h.table,
                    "files_ingested": stats.files_ingested,
                    "files_failed": stats.files_failed,
                    "last_event_ms_ago": last_event_ms_ago,
                    "last_error": stats.last_error,
                    "max_concurrent": stats.max_concurrent,
                    "in_flight": in_flight,
                })
            })
            .collect();
        Value::Array(items)
    }
}

impl Default for WatcherRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that increments `in_flight` on construction and decrements on
/// drop. Used to keep the live-load counter consistent even if an ingest
/// task panics or early-returns.
struct InFlightGuard {
    counter: Arc<AtomicU32>,
}

impl InFlightGuard {
    fn new(counter: Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "call-site ergonomics: function consumes logically-owned parameters, refactoring signatures is not worth per-site churn"
)]
/// Begin watching `dir`. Builds a dedicated connection pool, runs the
/// initial sweep (sequentially — there's no benefit to parallelism for
/// startup since the pool isn't under load yet), then installs a
/// [`notify`] watcher that streams events to an async tokio task.
/// Returns a snapshot of the initial-sweep stats.
///
/// The engine `Arc` must point to an already-initialized engine (the caller
/// in `server.rs` eagerly calls `ensure_engine` before invoking this).
///
/// # Errors
///
/// - Returns [`ErrorCode::FileNotFound`] if `dir` does not exist, is
///   not a directory, or cannot be canonicalized.
/// - Returns [`ErrorCode::InternalError`] if the watcher registry
///   mutex or engine mutex is poisoned, if the engine has not been
///   initialized, if `start_watching` is not called from a Tokio
///   runtime, or if the watcher pool / OS file-system watcher cannot
///   be constructed.
/// - Returns [`ErrorCode::InternalError`] wrapping the error string
///   when [`notify::RecommendedWatcher`] setup fails.
/// - Propagates any error from the initial sweep's per-file ingest
///   (file read, schema inference, or Hyper `COPY` / `INSERT` errors).
pub fn start_watching(
    engine: Arc<Mutex<Option<Engine>>>,
    registry: Arc<WatcherRegistry>,
    subscriptions: Option<Arc<SubscriptionRegistry>>,
    dir: PathBuf,
    table: String,
    options: WatchOptions,
) -> Result<WatcherStats, McpError> {
    if !dir.exists() {
        return Err(McpError::new(
            ErrorCode::FileNotFound,
            format!("Directory does not exist: {}", dir.display()),
        ));
    }
    if !dir.is_dir() {
        return Err(McpError::new(
            ErrorCode::FileNotFound,
            format!("Not a directory: {}", dir.display()),
        ));
    }
    let canonical = dir.canonicalize().map_err(|e| {
        McpError::new(
            ErrorCode::FileNotFound,
            format!("Cannot canonicalize {}: {e}", dir.display()),
        )
    })?;

    {
        let watchers = registry.watchers.lock().map_err(|_| {
            McpError::new(ErrorCode::InternalError, "Watcher registry lock poisoned")
        })?;
        if watchers.contains_key(&canonical) {
            return Err(McpError::new(
                ErrorCode::InternalError,
                format!("Already watching {}", canonical.display()),
            )
            .with_suggestion(
                "Call unwatch_directory first to re-register with different options",
            ));
        }
    }

    let concurrency = options.resolved_concurrency();

    // Build the per-watcher pool. We pull the endpoint and workspace path
    // from the engine under a brief lock, then release it — the pool
    // itself operates independently of the sync connection the engine
    // still owns.
    // Build the pool wrapped in an Arc<RwLock<…>> so post-construction
    // hyperd restarts can swap in a fresh pool without restarting the
    // watcher.
    let pool = Arc::new(tokio::sync::RwLock::new(build_watcher_pool(
        &engine,
        concurrency,
    )?));

    let stats = Arc::new(Mutex::new(WatcherStats {
        // Concurrency is configured by the user via a `u32`-sized field
        // upstream; saturating is a safe diagnostic.
        max_concurrent: u32::try_from(concurrency).unwrap_or(u32::MAX),
        ..Default::default()
    }));
    let in_flight = Arc::new(AtomicU32::new(0));
    // Set of `.ready` paths with an in-flight ingest task. Used to dedupe
    // duplicate filesystem events (macOS FSEvents in particular delivers
    // both Create and Modify events for a single `write` syscall, and
    // both would otherwise spawn independent ingest tasks — the per-task
    // `.exists()` check is a TOCTOU race, not a real idempotence guard).
    let in_flight_paths: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));

    // Initial sweep: process anything already in the directory before
    // wiring up events. Done sequentially on a single pooled connection
    // — fine, because the pool isn't under load yet and this keeps the
    // return-value shape simple (caller blocks on sweep completion).
    //
    // We run the sweep synchronously (the caller — an rmcp tool handler —
    // is a sync `fn` running on a multi-thread tokio runtime). A plain
    // `Handle::block_on` would panic with "Cannot start a runtime from
    // within a runtime"; `block_in_place` tells tokio to move this
    // worker thread off the task pool for the duration of the blocking
    // call, then resume. Requires the multi-thread flavor — which the
    // MCP binary uses via `#[tokio::main]` and which tests must opt
    // into with `#[tokio::test(flavor = "multi_thread")]`.
    let initial = {
        let rt = tokio::runtime::Handle::try_current().map_err(|_| {
            McpError::new(
                ErrorCode::InternalError,
                "start_watching must be called from inside a tokio runtime",
            )
        })?;
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                for ready_path in scan_ready_files(&canonical) {
                    process_ready_with_recovery(
                        &pool,
                        &engine,
                        concurrency,
                        subscriptions.as_deref(),
                        &canonical,
                        &table,
                        &ready_path,
                        &stats,
                    )
                    .await;
                }
                stats.lock().map(|s| s.snapshot()).unwrap_or_default()
            })
        })
    };

    // notify uses its own std channel; bridge it into a tokio mpsc via a
    // small forwarder thread so the async consumer can `.recv().await`.
    let (std_tx, std_rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let (async_tx, mut async_rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let _ = std_tx.send(res);
    })
    .map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("Failed to create watcher: {e}"),
        )
    })?;
    watcher
        .watch(&canonical, RecursiveMode::NonRecursive)
        .map_err(|e| {
            McpError::new(
                ErrorCode::InternalError,
                format!("Failed to watch directory: {e}"),
            )
        })?;

    // Forwarder thread: std sync -> tokio mpsc. Exits when the notify
    // watcher is dropped (the std sender goes away, `recv()` returns Err).
    let forwarder = {
        let async_tx = async_tx.clone();
        std::thread::Builder::new()
            .name(format!("hyperdb-mcp-watch-fwd-{}", canonical.display()))
            .spawn(move || {
                while let Ok(ev) = std_rx.recv() {
                    if async_tx.send(ev).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| {
                McpError::new(
                    ErrorCode::InternalError,
                    format!("Failed to spawn forwarder thread: {e}"),
                )
            })?
    };
    // Drop our local async sender so the consumer can actually reach EOF
    // once the forwarder thread exits (the forwarder keeps its own clone).
    drop(async_tx);

    // Consumer task: one per-ready-file ingest spawned as its own task,
    // bounded naturally by `pool.get().await` (the pool caps at
    // `concurrency`). We use `tokio::spawn` rather than `JoinSet` because
    // we don't need to await every task — they're fire-and-forget from
    // the consumer's point of view, with stats updated via shared Mutex.
    let task = {
        let pool = Arc::clone(&pool);
        let engine_for_pool = Arc::clone(&engine);
        let subs = subscriptions.clone();
        let stats = Arc::clone(&stats);
        let in_flight = Arc::clone(&in_flight);
        let in_flight_paths = Arc::clone(&in_flight_paths);
        let dir = canonical.clone();
        let table = table.clone();
        tokio::spawn(async move {
            while let Some(event_res) = async_rx.recv().await {
                let Ok(event) = event_res else { continue };
                if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                    continue;
                }
                for path in event.paths {
                    if !is_ready_file(&path) {
                        continue;
                    }
                    // Dedupe: if a task for this `.ready` path is already
                    // running, drop this event. A lost Modify-after-Create
                    // here is harmless — by the time the in-flight task
                    // reaches its own `.exists()` check the data file is
                    // either still there (gets ingested) or already moved
                    // to `failed/` (correctly skipped).
                    let claimed = in_flight_paths
                        .lock()
                        .is_ok_and(|mut set| set.insert(path.clone()));
                    if !claimed {
                        continue;
                    }
                    let pool_slot = Arc::clone(&pool);
                    let engine_handle = Arc::clone(&engine_for_pool);
                    let subs = subs.clone();
                    let stats = Arc::clone(&stats);
                    let in_flight = Arc::clone(&in_flight);
                    let in_flight_paths = Arc::clone(&in_flight_paths);
                    let dir = dir.clone();
                    let table = table.clone();
                    tokio::spawn(async move {
                        let _guard = InFlightGuard::new(in_flight);
                        process_ready_with_recovery(
                            &pool_slot,
                            &engine_handle,
                            concurrency,
                            subs.as_deref(),
                            &dir,
                            &table,
                            &path,
                            &stats,
                        )
                        .await;
                        if let Ok(mut set) = in_flight_paths.lock() {
                            set.remove(&path);
                        }
                    });
                }
            }
        })
    };

    let handle = WatcherHandle {
        directory: canonical.clone(),
        table,
        stats,
        in_flight,
        watcher: Some(watcher),
        task: Some(task),
        forwarder: Some(forwarder),
        _pool: pool,
    };
    {
        let mut watchers = registry.watchers.lock().map_err(|_| {
            McpError::new(ErrorCode::InternalError, "Watcher registry lock poisoned")
        })?;
        watchers.insert(canonical, handle);
    }
    Ok(initial)
}

/// Stop watching a directory. Returns a JSON summary including final stats.
///
/// # Errors
///
/// - Returns [`ErrorCode::InternalError`] if the watcher registry
///   mutex is poisoned.
/// - Returns [`ErrorCode::FileNotFound`] if no active watcher is
///   registered for the canonicalized `dir`.
pub fn stop_watching(registry: &WatcherRegistry, dir: &Path) -> Result<Value, McpError> {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());

    let handle_opt = {
        let mut watchers = registry.watchers.lock().map_err(|_| {
            McpError::new(ErrorCode::InternalError, "Watcher registry lock poisoned")
        })?;
        watchers.remove(&canonical)
    };

    match handle_opt {
        Some(handle) => {
            let stats = handle
                .stats
                .lock()
                .map(|s| s.snapshot())
                .unwrap_or_default();
            let directory = handle.directory.clone();
            let table = handle.table.clone();
            drop(handle); // triggers the Drop impl: stops watcher + joins thread
            Ok(json!({
                "directory": directory.to_string_lossy(),
                "table": table,
                "status": "stopped",
                "files_ingested": stats.files_ingested,
                "files_failed": stats.files_failed,
                "last_error": stats.last_error,
            }))
        }
        None => Err(McpError::new(
            ErrorCode::FileNotFound,
            format!("No active watcher for {}", canonical.display()),
        )
        .with_suggestion("Check status tool output for currently watched directories")),
    }
}

/// Scan `dir` (non-recursively) for files whose name ends with `.ready`.
fn scan_ready_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && is_ready_file(&path) {
            out.push(path);
        }
    }
    out
}

/// True if the path ends with the `.ready` sentinel suffix.
fn is_ready_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.ends_with(READY_SUFFIX))
}

/// Given a `.ready` sentinel path, return the paired data file path.
/// Returns `None` if the path doesn't end in `.ready`.
fn strip_ready_suffix(ready_path: &Path) -> Option<PathBuf> {
    let name = ready_path.file_name()?.to_str()?;
    let stripped = name.strip_suffix(READY_SUFFIX)?;
    Some(ready_path.with_file_name(stripped))
}

/// Ingest the data file paired with a `.ready` sentinel on a pooled
/// async connection. On success, both files are deleted. On failure,
/// both are moved to `<dir>/failed/` and a `<name>.error` JSON file is
/// written alongside.
///
/// The connection is checked out of the pool for the duration of the
/// ingest (including the `BEGIN / COMMIT`) and released on scope exit
/// via the `PooledConnection` Drop. Other ingest tasks run in parallel
/// on their own connections, up to the pool's `max_size`.
/// Ingest one ready file and return `Ok(rows)` on success, or the
/// underlying error. Pure ingest path — no file moves, no stats writes.
/// The wrapper layer ([`process_ready_with_recovery`]) handles those
/// after deciding whether the error is recoverable.
async fn ingest_one_ready_file(
    pool: &Arc<Pool>,
    table: &str,
    ready_path: &Path,
    data_path: &Path,
) -> Result<u64, McpError> {
    let conn = pool.get().await.map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("Failed to check out connection: {e}"),
        )
    })?;
    let opts = IngestOptions {
        table: table.to_string(),
        mode: "append".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let data_str = data_path
        .to_str()
        .ok_or_else(|| McpError::new(ErrorCode::InternalError, "Non-UTF-8 path"))?;
    let res = match detect_file_format(data_path) {
        InferredFileFormat::Parquet => ingest_parquet_file_async(&conn, data_str, &opts).await,
        InferredFileFormat::ArrowIpc => ingest_arrow_ipc_file_async(&conn, data_str, &opts).await,
        InferredFileFormat::Json => ingest_json_file_async(&conn, data_str, &opts).await,
        InferredFileFormat::Csv => ingest_csv_file_async(&conn, data_str, &opts).await,
    }?;
    let _ = ready_path; // silence the unused-variable lint; the path is used by the caller
    Ok(res.rows)
}

/// Ingest one ready file with one-shot pool-rebuild recovery. If the
/// first attempt fails with a connection-lost error (hyperd was
/// restarted by the daemon, or the pool's connections went stale), the
/// watcher rebuilds its pool from the engine's *current* endpoint and
/// retries the ingest exactly once. Persistent errors fall through to
/// the standard `failed/` move.
async fn process_ready_with_recovery(
    pool_slot: &tokio::sync::RwLock<Arc<Pool>>,
    engine: &Arc<Mutex<Option<Engine>>>,
    concurrency: usize,
    subscriptions: Option<&SubscriptionRegistry>,
    dir: &Path,
    table: &str,
    ready_path: &Path,
    stats: &Arc<Mutex<WatcherStats>>,
) {
    let Some(data_path) = strip_ready_suffix(ready_path) else {
        return;
    };
    // Idempotence: either file may already be gone (we processed it on a
    // previous event); skip silently.
    if !ready_path.exists() || !data_path.exists() {
        return;
    }
    let is_symlink = |p: &std::path::Path| {
        p.symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
    };
    if is_symlink(ready_path) || is_symlink(&data_path) {
        tracing::warn!(
            ready = %ready_path.display(),
            data = %data_path.display(),
            "Refusing to ingest: sentinel or data file is a symlink"
        );
        return;
    }

    // First attempt — uses whatever pool is currently in the slot.
    let active_pool = pool_slot.read().await.clone();
    let mut result = ingest_one_ready_file(&active_pool, table, ready_path, &data_path).await;
    drop(active_pool);

    // If the first attempt failed with what looks like a connection
    // loss, try rebuilding the pool once and retrying. Hyperd restarts
    // (daemon-managed) reuse the same endpoint slot but invalidate
    // every connection in the pool; without this branch the watcher
    // would route every subsequent file to `failed/` until the user
    // notices and re-issues `watch_directory`.
    if let Err(ref err) = result {
        if crate::error::is_connection_lost(&err.message) {
            tracing::warn!(
                err = %err.message,
                "watcher: detected connection-lost error, rebuilding pool and retrying"
            );
            match rebuild_watcher_pool(pool_slot, engine, concurrency).await {
                Ok(()) => {
                    let active_pool = pool_slot.read().await.clone();
                    result =
                        ingest_one_ready_file(&active_pool, table, ready_path, &data_path).await;
                }
                Err(e) => {
                    tracing::warn!(
                        err = %e.message,
                        "watcher: pool rebuild failed; the original ingest error will surface"
                    );
                }
            }
        }
    }

    match result {
        Ok(rows) => {
            let _ = std::fs::remove_file(ready_path);
            let _ = std::fs::remove_file(&data_path);
            if let Ok(mut s) = stats.lock() {
                s.files_ingested += 1;
                s.last_event_at = Some(SystemTime::now());
                s.last_error = None;
            }
            tracing::info!(
                "watcher: ingested {rows} rows from {} into {}",
                data_path.display(),
                table
            );
            if let Some(subs) = subscriptions {
                for uri in uris_for_table_change(table) {
                    subs.notify_updated(&uri);
                }
            }
        }
        Err(err) => {
            let fail_dir = dir.join("failed");
            let _ = std::fs::create_dir_all(&fail_dir);
            if let Some(name) = data_path.file_name() {
                let _ = std::fs::rename(&data_path, fail_dir.join(name));
                let err_file = fail_dir.join(format!("{}.error", name.to_string_lossy()));
                let err_json = serde_json::to_string_pretty(&json!({
                    "code": format!("{:?}", err.code),
                    "message": err.message,
                    "suggestion": err.suggestion,
                }))
                .unwrap_or_default();
                let _ = std::fs::write(err_file, err_json);
            }
            if let Some(name) = ready_path.file_name() {
                let _ = std::fs::rename(ready_path, fail_dir.join(name));
            }
            if let Ok(mut s) = stats.lock() {
                s.files_failed += 1;
                s.last_event_at = Some(SystemTime::now());
                s.last_error = Some(err.to_string());
            }
            tracing::warn!(
                "watcher: ingest failed for {}: {}",
                data_path.display(),
                err
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_ready_file_checks_suffix() {
        assert!(is_ready_file(Path::new("/tmp/foo.csv.ready")));
        assert!(is_ready_file(Path::new("/tmp/bar.ready")));
        assert!(!is_ready_file(Path::new("/tmp/foo.csv")));
        assert!(!is_ready_file(Path::new("/tmp/foo.ready.txt")));
    }

    #[test]
    fn strip_ready_gives_data_path() {
        assert_eq!(
            strip_ready_suffix(Path::new("/tmp/foo.csv.ready")).unwrap(),
            Path::new("/tmp/foo.csv")
        );
        assert!(strip_ready_suffix(Path::new("/tmp/foo.csv")).is_none());
    }

    #[test]
    fn resolved_concurrency_clamps() {
        assert_eq!(
            WatchOptions { max_concurrent: 0 }.resolved_concurrency(),
            DEFAULT_MAX_CONCURRENT
        );
        assert_eq!(WatchOptions { max_concurrent: 1 }.resolved_concurrency(), 1);
        assert_eq!(
            WatchOptions {
                max_concurrent: 1000
            }
            .resolved_concurrency(),
            MAX_CONCURRENT_LIMIT
        );
    }
}
