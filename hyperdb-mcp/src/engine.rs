// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Core database engine that owns the `HyperProcess` and its connection.
//!
//! The [`Engine`] is the single point of contact with the Hyper database. It
//! manages process startup, connection lifecycle, table DDL, query execution,
//! and workspace metadata. All higher-level modules (ingest, export, server)
//! operate through an `&Engine` reference.
//!
//! # Lazy Initialization and Connection Recovery
//!
//! The engine is lazily initialized by [`crate::server::HyperMcpServer`] on the
//! first tool call (not during MCP handshake). This keeps the `initialize`
//! response fast and avoids starting `hyperd` if the client never calls a tool.
//!
//! If the connection to `hyperd` is lost (crash, broken pipe, wire-protocol
//! desync), the server's `crate::server::HyperMcpServer::with_engine` wrapper
//! detects the [`crate::error::ErrorCode::ConnectionLost`] error, drops the
//! engine, and transparently re-creates it on the next call. This auto-reconnect
//! path covers both transport-level failures and the `"desynchronized"` state
//! surfaced by the `hyper-client` layer's bounded drain.
//!
//! # Workspace Model
//!
//! Every session has an **ephemeral primary database** at
//! `$TMPDIR/hyperdb-mcp-<pid>/scratch.hyper`. This is where unqualified
//! tool calls land — exploratory loads, ad-hoc queries, scratch tables.
//! It is created fresh on engine start and deleted (DETACH + remove) when
//! the engine drops.
//!
//! When a persistent path is supplied (CLI `--persistent-db`, env var
//! `HYPERDB_PERSISTENT_DB`, or the platform default), the engine records
//! it; the [`crate::server::HyperMcpServer`] then ATTACHes that file under
//! alias `"persistent"` after construction so the LLM can target it via
//! the `database` parameter on data tools, or via `persist: true` on
//! load tools. The persistent file lives across sessions.
//!
//! Passing `None` (or `--ephemeral-only` at the CLI) skips the persistent
//! attachment; the only available database is the ephemeral primary plus
//! any user-attached DBs.
//!
//! # Sync Calls in an Async Server
//!
//! All `Engine` methods are synchronous (blocking). The MCP server runs on a
//! tokio runtime, but `hyperd` communication goes through the `hyperdb-api` crate's
//! blocking `Connection` API. The `rmcp` framework spawns tool handlers on its
//! own task pool, so blocking calls do not starve the async event loop. A future
//! optimization could use `spawn_blocking` or an async connection API, but the
//! current approach is correct and simple.

use crate::daemon;
use crate::error::{ErrorCode, McpError};
use crate::schema::ColumnSchema;
use hyperdb_api::{
    escape_sql_path, Catalog, Connection, CreateMode, HyperProcess, Parameters, SqlType,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process counter so multiple `Engine` instances in the same PID get
/// distinct ephemeral directories (parallel test runners, embedded uses).
static EPHEMERAL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Reserved alias under which the default persistent database is attached.
/// Mirrored as [`Engine::PERSISTENT_ALIAS`] for the public API.
const PERSISTENT_ALIAS: &str = "persistent";

/// Outcome of [`attach_default_persistent`] — flags whether the file was
/// freshly created so the catalog-seed step can fire (or skip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistentAttachOutcome {
    /// `true` when MCP just created the `.hyper` file as part of the
    /// attach; `false` when the file already existed and we attached it
    /// as-is.
    pub file_was_created: bool,
}

/// Attach the persistent database under the reserved `"persistent"`
/// alias on `connection`, creating the underlying `.hyper` file if it
/// doesn't yet exist. Also pins `schema_search_path` to `primary_db_name`
/// so unqualified SQL keeps routing to the ephemeral primary.
fn attach_default_persistent(
    connection: &Connection,
    persistent_path: &Path,
    primary_db_name: &str,
) -> Result<PersistentAttachOutcome, McpError> {
    let path_str = persistent_path.to_string_lossy();
    let file_was_created = !persistent_path.exists();
    if file_was_created {
        let create_sql = format!(
            "CREATE DATABASE IF NOT EXISTS {}",
            escape_sql_path(&path_str)
        );
        connection.execute_command(&create_sql).map_err(|e| {
            McpError::new(
                ErrorCode::InternalError,
                format!("Failed to create persistent database: {e}"),
            )
        })?;
    }
    let attach_sql = format!(
        "ATTACH DATABASE {path} AS \"{alias}\"",
        path = escape_sql_path(&path_str),
        alias = PERSISTENT_ALIAS,
    );
    connection.execute_command(&attach_sql).map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("Failed to attach persistent database: {e}"),
        )
    })?;
    // Pin search_path to the primary so unqualified SQL keeps routing
    // there even with the persistent attachment present. Mirrors the
    // logic AttachRegistry uses for user-attached databases.
    let pin_sql = format!(
        "SET schema_search_path = '{}'",
        primary_db_name.replace('\'', "''")
    );
    connection.execute_command(&pin_sql).map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("Failed to pin schema_search_path: {e}"),
        )
    })?;
    Ok(PersistentAttachOutcome { file_was_created })
}

/// File-stem of a `.hyper` path as the unqualified database name Hyper
/// uses internally. Falls back to `"scratch"` if the stem can't be read.
fn path_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("scratch")
        .to_string()
}

/// Owns a connection to `hyperd`, the ephemeral primary database, and an
/// optional persistent attachment path. All SQL execution flows through
/// this struct.
///
/// Two process modes:
/// - **Local** — this engine owns the `HyperProcess` subprocess directly.
/// - **Daemon** — a shared daemon manages `hyperd`; the engine only holds a connection.
///
/// Database layout:
/// RAII guard that restores the `schema_search_path` to the primary
/// database when dropped. Created by [`Engine::scoped_search_path`].
/// If the restore fails, logs a warning — the engine mutex serializes
/// calls so the stale path only persists until the next tool call's
/// own `scoped_search_path` or until `with_engine` replaces the engine
/// on a `ConnectionLost` error.
#[derive(Debug)]
pub struct ScopedSearchPath<'a> {
    engine: &'a Engine,
    restore_to: String,
}

impl Drop for ScopedSearchPath<'_> {
    fn drop(&mut self) {
        let sql = format!(
            "SET schema_search_path = '{}'",
            self.restore_to.replace('\'', "''")
        );
        if let Err(e) = self.engine.execute_command(&sql) {
            tracing::warn!(
                error = %e.message,
                "failed to restore schema_search_path — next tool call may route incorrectly"
            );
        }
    }
}

/// - The connection is *bound* to the ephemeral primary at
///   [`Self::ephemeral_path`]. Unqualified SQL routes here.
/// - When [`Self::persistent_path`] is `Some`, the server attaches that
///   file as `"persistent"` after engine construction. When `None`, no
///   persistent storage is available this session (`--ephemeral-only`).
#[derive(Debug)]
pub struct Engine {
    /// `None` in daemon mode (the daemon owns the process).
    hyper: Option<HyperProcess>,
    /// Stored endpoint for daemon mode (the daemon advertises this).
    daemon_endpoint: Option<String>,
    connection: Connection,
    /// The primary database for this session. Lives in a temp dir and is
    /// deleted on `Drop`.
    ephemeral_path: PathBuf,
    /// User-data persistent database. Attached under alias `"persistent"`
    /// during [`Engine::new`]. `None` in `--ephemeral-only` mode.
    persistent_path: Option<PathBuf>,
    /// `true` when the persistent `.hyper` file was just created during
    /// engine construction (so the catalog-seed step should fire). Reset
    /// to `false` after the server consumes it via
    /// [`Self::take_persistent_was_created`].
    persistent_was_created: bool,
    /// Cached "_table_catalog exists in `<alias>`" probes, keyed by
    /// canonical alias (lowercase). Populated on first call to
    /// [`Self::catalog_present_in`] for each `(engine, alias)` pair.
    ///
    /// Lives on the Engine because the catalog is per-engine-lifetime
    /// (a `ConnectionLost` reconnect creates a fresh Engine, so the
    /// cache resets at the right boundary). Detaching an alias clears
    /// its entry via [`Self::clear_catalog_cache_for`] so a re-attach
    /// to a different file/writability doesn't reuse a stale value.
    /// `Some(false)` is cacheable too — once the catalog is confirmed
    /// absent it stays absent for the rest of the engine's lifetime
    /// unless explicitly cleared.
    catalog_present_cache: std::sync::Mutex<std::collections::HashMap<String, bool>>,
    log_dir: PathBuf,
}

impl Engine {
    /// Create a new Engine. The connection is bound to a fresh ephemeral
    /// primary in a temp directory. If `persistent_db_path` is `Some`,
    /// the path is recorded so the server can ATTACH it post-construction;
    /// passing `None` means `--ephemeral-only`.
    ///
    /// Connects to the shared daemon if available, falling back to a local `hyperd`.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorCode::PermissionDenied`] if the persistent parent
    ///   directory or the log directory cannot be created.
    /// - Returns [`ErrorCode::InternalError`] if the ephemeral temp
    ///   directory cannot be created, if the `public` schema bootstrap
    ///   fails, or if the initial connection to `hyperd` fails.
    /// - Returns [`ErrorCode::HyperdNotFound`] when [`HyperProcess::new`]
    ///   reports the `hyperd` executable is missing or unreachable via
    ///   `HYPERD_PATH`.
    pub fn new(persistent_db_path: Option<String>) -> Result<Self, McpError> {
        Self::new_with_mode(persistent_db_path, false)
    }

    /// Create an engine that bypasses the shared daemon and spawns a private `hyperd`.
    ///
    /// # Errors
    /// Same as [`Self::new`].
    pub fn new_no_daemon(persistent_db_path: Option<String>) -> Result<Self, McpError> {
        Self::new_with_mode(persistent_db_path, true)
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "Option<String> is consumed by the path-expansion logic below"
    )]
    fn new_with_mode(
        persistent_db_path: Option<String>,
        no_daemon: bool,
    ) -> Result<Self, McpError> {
        // Resolve persistent path (if requested) and pre-create its parent dir.
        let persistent_path = match persistent_db_path.as_deref() {
            Some(p) => {
                let path = PathBuf::from(shellexpand_tilde(p));
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        McpError::new(
                            ErrorCode::PermissionDenied,
                            format!("Cannot create persistent-db directory: {e}"),
                        )
                    })?;
                }
                Some(path)
            }
            None => None,
        };

        // Always allocate a fresh ephemeral primary in a per-engine temp dir.
        // The directory name combines the PID and a process-wide counter so
        // multiple Engine instances in the same process (parallel tests,
        // embedded uses, restart-after-ConnectionLost) never collide.
        let seq = EPHEMERAL_SEQ.fetch_add(1, Ordering::Relaxed);
        let ephemeral_dir =
            std::env::temp_dir().join(format!("hyperdb-mcp-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&ephemeral_dir).map_err(|e| {
            McpError::new(
                ErrorCode::InternalError,
                format!("Cannot create ephemeral directory: {e}"),
            )
        })?;
        let ephemeral_path = ephemeral_dir.join("scratch.hyper");

        // Logs live next to the persistent file when one was supplied so
        // operators find them in a stable location; otherwise next to the
        // ephemeral primary.
        let log_dir = resolve_log_dir(persistent_db_path.as_deref());
        std::fs::create_dir_all(&log_dir).map_err(|e| {
            McpError::new(
                ErrorCode::PermissionDenied,
                format!("Cannot create log directory {}: {e}", log_dir.display()),
            )
        })?;

        // Try daemon mode first unless disabled
        if !no_daemon {
            if let Some(engine) =
                Self::try_daemon_mode(&ephemeral_path, persistent_path.clone(), &log_dir)?
            {
                return Ok(engine);
            }
        }

        // Fall back to spawning a local HyperProcess
        let mut params = Parameters::new();
        params.set("log_file_max_count", "2");
        params.set("log_file_size_limit", "100M");
        params.set("log_dir", log_dir.to_string_lossy().as_ref());

        let hyper = HyperProcess::new(None, Some(&params)).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("hyperd") || msg.contains("HYPERD_PATH") || msg.contains("No such file")
            {
                McpError::new(ErrorCode::HyperdNotFound, msg)
            } else {
                McpError::new(ErrorCode::InternalError, msg)
            }
        })?;

        // Bind to the ephemeral primary. CreateAndReplace because a stale
        // file in the per-pid temp dir from a crashed prior session would
        // otherwise leak into this one.
        let connection = Connection::new(&hyper, &ephemeral_path, CreateMode::CreateAndReplace)
            .map_err(|e| {
                McpError::new(ErrorCode::InternalError, format!("Failed to connect: {e}"))
            })?;

        bootstrap_public_schema(&connection)?;

        let primary_db_name = path_stem(&ephemeral_path);
        let persistent_was_created = Self::attach_persistent_if_present(
            &connection,
            persistent_path.as_deref(),
            &primary_db_name,
        )?;

        Ok(Self {
            hyper: Some(hyper),
            daemon_endpoint: None,
            connection,
            ephemeral_path,
            persistent_path,
            persistent_was_created,
            catalog_present_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            log_dir,
        })
    }

    /// If `persistent_path` is `Some`, attach the file under the reserved
    /// `"persistent"` alias and pin the search path. Returns `true` if
    /// the file was just created, `false` if it already existed or if
    /// `persistent_path` is `None`.
    fn attach_persistent_if_present(
        connection: &Connection,
        persistent_path: Option<&Path>,
        primary_db_name: &str,
    ) -> Result<bool, McpError> {
        let Some(path) = persistent_path else {
            return Ok(false);
        };
        let outcome = attach_default_persistent(connection, path, primary_db_name)?;
        Ok(outcome.file_was_created)
    }

    /// Attempt to connect via the shared daemon. Returns `None` if the daemon
    /// cannot be reached (falls back to local mode).
    fn try_daemon_mode(
        ephemeral_path: &Path,
        persistent_path: Option<PathBuf>,
        log_dir: &Path,
    ) -> Result<Option<Self>, McpError> {
        let port = daemon::discovery::resolve_port();
        let info = match daemon::spawn::ensure_daemon(port) {
            Ok(info) => info,
            Err(e) => {
                tracing::debug!(error = %e, "daemon unavailable, falling back to local mode");
                return Ok(None);
            }
        };

        let endpoint = &info.hyperd_endpoint;
        // CreateAndReplace: same rationale as the local path — a per-pid
        // temp file from a crashed prior session shouldn't leak in.
        let connection = Connection::connect(
            endpoint,
            &ephemeral_path.to_string_lossy(),
            CreateMode::CreateAndReplace,
        )
        .map_err(|e| {
            // The daemon's discovery file points at this endpoint but we can't
            // reach it — hyperd is likely dead. Tell the daemon so it can
            // restart it on its next monitor tick.
            daemon::health::report_hyperd_error_to_daemon();
            McpError::new(
                ErrorCode::InternalError,
                format!("Failed to connect to daemon hyperd at {endpoint}: {e}"),
            )
        })?;

        bootstrap_public_schema(&connection)?;

        // Send heartbeat so daemon knows we're active
        let _ = daemon::health::send_command(info.health_port, "HEARTBEAT");

        let primary_db_name = path_stem(ephemeral_path);
        let persistent_was_created = Self::attach_persistent_if_present(
            &connection,
            persistent_path.as_deref(),
            &primary_db_name,
        )?;

        Ok(Some(Self {
            hyper: None,
            daemon_endpoint: Some(info.hyperd_endpoint),
            connection,
            ephemeral_path: ephemeral_path.to_path_buf(),
            persistent_path,
            persistent_was_created,
            catalog_present_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            log_dir: log_dir.to_path_buf(),
        }))
    }

    /// Whether the backing `hyperd` process is still alive.
    /// In daemon mode, checks the daemon health port.
    pub fn is_running(&self) -> bool {
        if let Some(ref hyper) = self.hyper {
            hyper.is_running()
        } else {
            // Daemon mode: check if daemon is still reachable
            daemon::discovery::discover().is_some()
        }
    }

    /// `host:port` endpoint of the `hyperd` process. Used by the
    /// watcher to build additional async connections via `hyperdb_api::pool`
    /// without touching the primary sync connection this engine holds.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InternalError`] if the endpoint is unavailable.
    pub fn hyperd_endpoint(&self) -> Result<String, McpError> {
        if let Some(ref endpoint) = self.daemon_endpoint {
            return Ok(endpoint.clone());
        }
        self.hyper
            .as_ref()
            .ok_or_else(|| {
                McpError::new(
                    ErrorCode::InternalError,
                    "no hyperd endpoint available".to_string(),
                )
            })?
            .require_endpoint()
            .map(std::string::ToString::to_string)
            .map_err(|e| McpError::new(ErrorCode::InternalError, e.to_string()))
    }

    /// Absolute path to the ephemeral primary `.hyper` file on disk.
    pub fn ephemeral_path(&self) -> &Path {
        &self.ephemeral_path
    }

    /// Absolute path to the persistent `.hyper` file, or `None` when the
    /// session is `--ephemeral-only`.
    pub fn persistent_path(&self) -> Option<&Path> {
        self.persistent_path.as_deref()
    }

    /// Reserved alias under which the persistent database is attached
    /// when [`Self::persistent_path`] is set. Visible to the LLM via the
    /// `database` parameter and via `list_attached_databases`.
    pub const PERSISTENT_ALIAS: &'static str = "persistent";

    /// Unqualified database name Hyper uses for the ephemeral primary —
    /// the stem of [`Self::ephemeral_path`]. Matches what
    /// [`hyperdb_api::Connection::new`] registers when it issues its
    /// implicit `ATTACH DATABASE`, so fully-qualified SQL built with this
    /// value resolves to the primary.
    ///
    /// Also the correct value for `SET schema_search_path = '…'` while
    /// additional databases are attached: Hyper's default search path
    /// (`"$single"`) only covers the implicit primary when no other
    /// databases are attached, and starts resolving unqualified names to
    /// nothing the moment an `ATTACH DATABASE` runs.
    pub fn primary_db_name(&self) -> String {
        self.ephemeral_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("scratch")
            .to_string()
    }

    /// Resolve a tool's optional `database` parameter to a concrete
    /// alias suitable for fully-qualifying SQL. `None` and `Some("")`
    /// mean "the primary (ephemeral)"; `Some("persistent")` requires the
    /// persistent attachment exists; any other value is returned
    /// verbatim and assumed to be a user-attached alias.
    ///
    /// Returns the database alias to qualify against, or `None` to mean
    /// "use the primary's name". This lets callers build qualified SQL
    /// uniformly: `format!("\"{}\".\"public\".\"{}\"", alias_or_primary, table)`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidArgument`] when `Some("persistent")`
    /// is passed but [`Self::persistent_path`] is `None`
    /// (`--ephemeral-only` mode).
    pub fn resolve_target_db(&self, requested: Option<&str>) -> Result<String, McpError> {
        match requested.map(str::trim) {
            None | Some("") => Ok(self.primary_db_name()),
            Some(other) if other.eq_ignore_ascii_case(Self::PERSISTENT_ALIAS) => {
                if self.persistent_path.is_none() {
                    return Err(McpError::new(
                        ErrorCode::InvalidArgument,
                        "no persistent database in this session — \
                         hyperdb-mcp was started with --ephemeral-only"
                            .to_string(),
                    ));
                }
                // Canonicalize to the lowercase form so SQL identifiers
                // and attachment registry lookups always agree.
                Ok(Self::PERSISTENT_ALIAS.to_string())
            }
            // Non-persistent aliases are also canonicalized to lowercase
            // so qualified SQL like `"alias"."public"."t"` matches the
            // ATTACH form, which `AttachRegistry::attach` lowercases.
            // Without this, `database="MyDB"` would build qualified SQL
            // referring to `"MyDB"` while the engine attached as
            // `"mydb"`, and Hyper (case-sensitive on quoted identifiers)
            // would reject the lookup.
            Some(other) => Ok(other.to_ascii_lowercase()),
        }
    }

    /// Temporarily redirect the schema search path to `alias` for the
    /// duration of a tool call. Returns an RAII guard that restores the
    /// search path to the primary when dropped.
    ///
    /// The engine `Mutex` is held by the caller (`with_engine` closure),
    /// so concurrent tool calls cannot observe the redirected path.
    ///
    /// # Errors
    ///
    /// Returns [`McpError`] if the SET statement fails (e.g. invalid alias
    /// or connection lost).
    pub fn scoped_search_path(&self, alias: &str) -> Result<ScopedSearchPath<'_>, McpError> {
        let primary = self.primary_db_name();
        let set_sql = format!("SET schema_search_path = '{}'", alias.replace('\'', "''"));
        self.execute_command(&set_sql)?;
        Ok(ScopedSearchPath {
            engine: self,
            restore_to: primary,
        })
    }

    /// Directory where `hyperd` writes its log files. The MCP binary should
    /// also drop its own client-side log here so debugging starts in one
    /// place.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Best-guess path to the most recent `hyperd` log file, useful when
    /// something in the engine misbehaves and we want to surface the server
    /// log to the caller. Picks the newest `hyperd*.log` file in [`log_dir`].
    /// Returns `None` if no matching file exists yet.
    ///
    /// [`log_dir`]: Self::log_dir
    pub fn hyperd_log_path(&self) -> Option<PathBuf> {
        let entries = std::fs::read_dir(&self.log_dir).ok()?;
        let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|e| {
                let path = e.path();
                let name = path.file_name()?.to_str()?;
                if name.starts_with("hyperd")
                    && std::path::Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
                {
                    let mtime = e.metadata().ok().and_then(|m| m.modified().ok())?;
                    Some((mtime, path))
                } else {
                    None
                }
            })
            .collect();
        candidates.sort_by_key(|b| std::cmp::Reverse(b.0));
        candidates.into_iter().next().map(|(_, p)| p)
    }

    /// `true` if a persistent database is attached to this session.
    /// Equivalent to [`Self::persistent_path`] being `Some`.
    pub fn has_persistent(&self) -> bool {
        self.persistent_path.is_some()
    }

    /// `true` when this engine just created the persistent `.hyper` file
    /// during construction. The server consumes this signal once to
    /// decide whether to seed `_table_catalog`; subsequent reads stay
    /// `true` (the flag isn't reset — it's a fact about the engine's
    /// startup, not a one-shot signal).
    pub fn persistent_was_just_created(&self) -> bool {
        self.persistent_was_created
    }

    /// Returns whether `_table_catalog` exists in `alias`, caching
    /// the per-DB result on first call so subsequent catalog read/
    /// write paths skip the `pg_catalog.pg_tables` probe.
    ///
    /// `prober` is the SQL-side existence check; the cache layer here
    /// is intentionally generic so the catalog module can keep its
    /// probe SQL in one place.
    ///
    /// # Errors
    /// Propagates whatever error `prober` returns on the first call.
    /// On subsequent calls, the cached value is returned without
    /// re-running the probe.
    pub fn catalog_present_in<F>(&self, alias: &str, prober: F) -> Result<bool, McpError>
    where
        F: Fn(&Engine) -> Result<bool, McpError>,
    {
        let key = alias.to_ascii_lowercase();
        // Fast path: cache already populated.
        if let Ok(guard) = self.catalog_present_cache.lock() {
            if let Some(&present) = guard.get(&key) {
                return Ok(present);
            }
        }
        // Slow path: run the probe and cache its result.
        let present = prober(self)?;
        if let Ok(mut guard) = self.catalog_present_cache.lock() {
            guard.insert(key, present);
        }
        Ok(present)
    }

    /// Synchronously set the catalog-presence cache to `true` for
    /// `alias` — used by `table_catalog::ensure_exists_in` after a
    /// successful `CREATE TABLE IF NOT EXISTS` so subsequent reads/
    /// writes against that DB skip the existence probe.
    pub fn mark_catalog_present_for(&self, alias: &str) {
        let key = alias.to_ascii_lowercase();
        if let Ok(mut guard) = self.catalog_present_cache.lock() {
            guard.insert(key, true);
        }
    }

    /// Drop the cached probe result for `alias`. Called by
    /// `detach_database` so that re-attaching the same alias to a
    /// different file (or with different writability) doesn't reuse a
    /// stale entry.
    pub fn clear_catalog_cache_for(&self, alias: &str) {
        let key = alias.to_ascii_lowercase();
        if let Ok(mut guard) = self.catalog_present_cache.lock() {
            guard.remove(&key);
        }
    }

    /// Direct access to the underlying connection for operations not
    /// wrapped by `Engine` (e.g. `export_csv`, `execute_query_to_arrow`).
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Execute a DDL/DML command. Returns affected row count.
    ///
    /// # Errors
    ///
    /// Converts any [`hyperdb_api::Error`] from the underlying connection
    /// into an [`McpError`] — typical causes are SQL syntax errors,
    /// constraint violations, permission failures, or
    /// [`ErrorCode::ConnectionLost`] when the link to `hyperd` has
    /// dropped.
    pub fn execute_command(&self, sql: &str) -> Result<u64, McpError> {
        self.connection.execute_command(sql).map_err(McpError::from)
    }

    /// Run the given closure inside a database transaction.
    ///
    /// Issues `BEGIN TRANSACTION` before calling `f`. If `f` returns `Ok`,
    /// commits the transaction; if it returns `Err`, rolls back and returns
    /// the original error. A failed rollback is logged via `tracing::warn!`
    /// and the original error is still surfaced (rollback failure usually
    /// means the transaction was already aborted by the server, which is
    /// functionally equivalent to a successful rollback).
    ///
    /// This is the correctness primitive for ingest operations: it lets
    /// per-row `INSERT` loops (Parquet, Arrow, JSON) leave zero partial data
    /// on failure. The CSV `COPY FROM` path is already atomic at the
    /// statement level, but wrapping it in a transaction costs nothing and
    /// makes per-row INSERT loops atomic across the whole batch.
    ///
    /// # DDL is auto-committed
    ///
    /// Hyper treats `DROP TABLE` and `CREATE TABLE` as auto-committed even
    /// when issued inside a transaction. This means `replace`-mode ingest
    /// cannot roll back the original table once DDL has run. The guarantee
    /// is weaker than it looks: on failure, the new (empty) table stays
    /// in place rather than being replaced by partial data. Append-mode
    /// ingest is fully atomic because it doesn't issue DDL on existing
    /// tables.
    ///
    /// # Known wire protocol quirk
    ///
    /// After a mid-transaction Hyper-level error (e.g. a NOT NULL violation
    /// on INSERT), the first SELECT after rollback may return an empty
    /// result set due to residual bytes on the connection. Retrying the
    /// query once restores normal behavior. The rollback itself is always
    /// correct — this is a read-side symptom only. See the `query_resilient`
    /// helper in `tests/transaction_tests.rs` for a robust pattern.
    ///
    /// # Errors
    ///
    /// - Returns any [`McpError`] raised by `BEGIN TRANSACTION` or by
    ///   `COMMIT` (typical causes: connection loss, serialization
    ///   conflict, DDL auto-commit contention).
    /// - Returns whatever error `f` produces (rollback is performed
    ///   first; a rollback failure is only logged, never surfaced).
    ///
    /// # Panics
    ///
    /// Does not introduce new panic sites. If `f` panics, the transaction
    /// is rolled back (best-effort) and the original panic is re-raised
    /// via [`std::panic::resume_unwind`], preserving the panic payload.
    pub fn execute_in_transaction<F, T>(&self, f: F) -> Result<T, McpError>
    where
        F: FnOnce(&Engine) -> Result<T, McpError>,
    {
        self.connection
            .begin_transaction()
            .map_err(McpError::from)?;
        tracing::debug!("tx: BEGIN issued");
        // `catch_unwind` wraps the closure so a panic (unwrap on None,
        // indexing OOB, arithmetic overflow, …) doesn't leave an open
        // transaction on the connection. Without this, the next tool
        // call would hit "transaction already in progress" and the
        // server's ConnectionLost auto-reconnect would *not* recover
        // because the connection is live; the engine would stay wedged
        // until restart. `AssertUnwindSafe` is correct here: we hold
        // the transaction open for the closure's duration, and we
        // always issue a rollback before resuming the panic, so no
        // logical invariant survives into the panicking stack.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        match result {
            Ok(Ok(val)) => {
                tracing::debug!("tx: closure returned Ok, issuing COMMIT");
                self.connection.commit().map_err(McpError::from)?;
                Ok(val)
            }
            Ok(Err(e)) => {
                tracing::debug!(err = %e, "tx: closure returned Err, issuing ROLLBACK");
                if let Err(rb_err) = self.connection.rollback() {
                    // Rollback itself failed — log it but keep the original
                    // error as the primary cause. A failed rollback usually
                    // means the transaction was already aborted by the server,
                    // which is fine (nothing to unwind).
                    tracing::warn!(
                        "rollback after error failed (original error preserved): {}",
                        rb_err
                    );
                } else {
                    tracing::debug!("tx: ROLLBACK succeeded");
                }
                Err(e)
            }
            Err(panic_payload) => {
                tracing::error!("tx: closure panicked, issuing ROLLBACK before resuming unwind");
                // Best-effort rollback. If it fails, the connection is
                // unusable — but we're about to panic anyway, and
                // `HyperMcpServer::with_engine` will drop the engine
                // when the panic surfaces as a poisoned tokio task.
                let _ = self.connection.rollback();
                std::panic::resume_unwind(panic_payload)
            }
        }
    }

    /// Execute a SELECT query and materialize all result rows as a JSON array
    /// of `{column_name: value}` objects.
    ///
    /// Results are consumed chunk-by-chunk to avoid holding the entire result
    /// set in protocol buffers, though the final `Vec<Value>` does accumulate
    /// in memory. For truly huge results, prefer `export` to a file instead.
    ///
    /// # Errors
    ///
    /// Returns any [`McpError`] produced by [`Connection::execute_query`]
    /// or subsequent `next_chunk` calls — SQL errors, connection loss,
    /// and decoding failures all surface through this path.
    pub fn execute_query_to_json(&self, sql: &str) -> Result<Vec<Value>, McpError> {
        let mut result = self.connection.execute_query(sql).map_err(McpError::from)?;

        let mut rows_json = Vec::new();
        let mut schema_opt = None;
        while let Some(chunk) = result.next_chunk().map_err(McpError::from)? {
            // Capture schema from first chunk
            if schema_opt.is_none() {
                schema_opt = result.schema();
            }
            if let Some(ref schema) = schema_opt {
                let columns = schema.columns();
                for row in &chunk {
                    let mut obj = serde_json::Map::new();
                    for col in columns {
                        let val = row_value_to_json(row, col.index(), &col.sql_type());
                        obj.insert(col.name().to_string(), val);
                    }
                    rows_json.push(Value::Object(obj));
                }
            }
        }
        Ok(rows_json)
    }

    /// Create a table from a schema definition.
    ///
    /// - `replace = true`: drops the existing table (if any) and recreates it.
    ///   Old rows are lost. Schema is defined by `columns`.
    /// - `replace = false` (append mode): creates the table only if it doesn't
    ///   already exist. If it does exist, the schema defined here is ignored
    ///   and subsequent inserts must match the existing schema.
    ///
    /// Uses `CREATE TABLE IF NOT EXISTS` / `DROP TABLE IF EXISTS` so the
    /// operation is idempotent without needing a separate `has_table` probe.
    /// This is important for the watcher path, where a racy `has_table` check
    /// (false negative due to protocol desync) would otherwise attempt a bare
    /// `CREATE TABLE` that fails with "42P07 table already exists" and leaves
    /// the connection in an aborted state.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorCode::EmptyData`] if `columns` is empty.
    /// - Returns [`ErrorCode::SchemaMismatch`] if any column's
    ///   `hyper_type` cannot be resolved by [`crate::schema::map_hyper_type`].
    /// - Propagates any Hyper error from `DROP TABLE` (when `replace`
    ///   is true) or `CREATE TABLE IF NOT EXISTS`.
    pub fn create_table(
        &self,
        table_name: &str,
        columns: &[ColumnSchema],
        replace: bool,
    ) -> Result<(), McpError> {
        self.create_table_in(table_name, columns, replace, None)
    }

    /// Create a table, optionally in a non-primary database. When
    /// `target_db` is `Some`, the table identifier is fully qualified as
    /// `"db"."public"."table"`; when `None`, it's just `"table"`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::create_table`].
    pub fn create_table_in(
        &self,
        table_name: &str,
        columns: &[ColumnSchema],
        replace: bool,
        target_db: Option<&str>,
    ) -> Result<(), McpError> {
        if columns.is_empty() {
            return Err(McpError::new(
                ErrorCode::EmptyData,
                "No columns to create table from",
            ));
        }
        for col in columns {
            if crate::schema::map_hyper_type(&col.hyper_type).is_none() {
                return Err(McpError::new(
                    ErrorCode::SchemaMismatch,
                    format!(
                        "Unknown type '{}' for column '{}'",
                        col.hyper_type, col.name
                    ),
                ));
            }
        }

        let quoted_table = match target_db {
            Some(db) => {
                let esc_db = db.replace('"', "\"\"");
                let esc_tbl = table_name.replace('"', "\"\"");
                format!("\"{esc_db}\".\"public\".\"{esc_tbl}\"")
            }
            None => format!("\"{}\"", table_name.replace('"', "\"\"")),
        };
        if replace {
            self.connection
                .execute_command(&format!("DROP TABLE IF EXISTS {quoted_table}"))
                .map_err(McpError::from)?;
        }

        let col_defs: Vec<String> = columns
            .iter()
            .map(|c| {
                let nullable = if c.nullable { "" } else { " NOT NULL" };
                format!(
                    "\"{}\" {}{}",
                    c.name.replace('"', "\"\""),
                    c.hyper_type,
                    nullable
                )
            })
            .collect();

        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {} ({})",
            quoted_table,
            col_defs.join(", ")
        );
        self.connection
            .execute_command(&create_sql)
            .map_err(McpError::from)?;
        Ok(())
    }

    /// Returns `(name, hyper_type, nullable)` for every column of `table`,
    /// in declaration order, by reading the catalog (the same path
    /// `describe_table` uses). Used by the `merge` ingest path to
    /// compare incoming-file schema against the existing table.
    ///
    /// # Errors
    ///
    /// - Propagates [`Catalog::get_table_definition`] errors. Callers
    ///   that need a "table missing" sentinel should pre-check via
    ///   `Catalog::get_table_names("public")` (see `describe_table` for
    ///   the precedent) — `get_table_definition` errors with a
    ///   variable wording across Hyper versions.
    pub fn column_metadata(&self, table: &str) -> Result<Vec<ColumnSchema>, McpError> {
        let catalog = Catalog::new(&self.connection);
        let def = catalog
            .get_table_definition(table)
            .map_err(McpError::from)?;
        Ok(def
            .columns()
            .iter()
            .map(|c| ColumnSchema {
                name: c.name.clone(),
                hyper_type: c.type_name().to_string(),
                nullable: c.nullable,
            })
            .collect())
    }

    /// Like [`Self::column_metadata`] but for a table in `target_db`.
    /// `None` falls back to `column_metadata` (primary). `Some(alias)`
    /// reads via the qualified `pg_catalog.pg_attribute` join used by
    /// `describe_columns_via_pg_catalog` — the connection-bound
    /// `Catalog` API can't see attached databases.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::TableNotFound`] when no rows come back from
    /// the qualified probe. Propagates connection errors.
    pub fn column_metadata_in(
        &self,
        target_db: Option<&str>,
        table: &str,
    ) -> Result<Vec<ColumnSchema>, McpError> {
        let Some(db) = target_db else {
            return self.column_metadata(table);
        };
        let rows = describe_columns_via_pg_catalog(self, db, table)?;
        if rows.is_empty() {
            return Err(McpError::new(
                ErrorCode::TableNotFound,
                format!("Table '{table}' does not exist in database '{db}'"),
            ));
        }
        Ok(rows
            .into_iter()
            .map(|r| ColumnSchema {
                name: r
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                hyper_type: r
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                nullable: r
                    .get("nullable")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true),
            })
            .collect())
    }

    /// Returns true if `table` exists in the `public` schema. Avoids
    /// the per-version error-string ambiguity of
    /// [`Catalog::get_table_definition`] by listing names instead.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`Catalog::get_table_names`] (typically
    /// connection loss).
    pub fn table_exists(&self, table: &str) -> Result<bool, McpError> {
        let catalog = Catalog::new(&self.connection);
        let names = catalog.get_table_names("public").map_err(McpError::from)?;
        Ok(names.iter().any(|n| n.as_str() == table))
    }

    /// Like [`Self::table_exists`] but for a table in `target_db`.
    /// `None` falls back to `table_exists` (primary). `Some(alias)`
    /// probes the qualified `pg_catalog.pg_tables` of the attached
    /// database — the connection-bound `Catalog` API can't see
    /// attached databases.
    ///
    /// # Errors
    ///
    /// Propagates connection errors from the probe query.
    pub fn table_exists_in(&self, target_db: Option<&str>, table: &str) -> Result<bool, McpError> {
        let Some(db) = target_db else {
            return self.table_exists(table);
        };
        let esc_db = db.replace('"', "\"\"");
        let esc_tbl = table.replace('\'', "''");
        let sql = format!(
            "SELECT 1 AS one FROM \"{esc_db}\".pg_catalog.pg_tables \
             WHERE schemaname = 'public' AND tablename = '{esc_tbl}'"
        );
        let rows = self.execute_query_to_json(&sql)?;
        Ok(!rows.is_empty())
    }

    /// Issue a single `ALTER TABLE "<table>" ADD COLUMN "<n1>" <t1>,
    /// ADD COLUMN "<n2>" <t2>, …` statement that adds all columns
    /// atomically. Hyper supports the multi-column form (verified
    /// 2026-05-07 against the pinned hyperd release), so partial-add
    /// failures don't leave the schema half-widened.
    ///
    /// New columns are always added nullable — existing rows have no
    /// value to satisfy NOT NULL. `nullable` on the input is ignored
    /// for that reason.
    ///
    /// `cols` must be non-empty; an empty input is a no-op (returns
    /// `Ok(())` without issuing SQL) so callers can pass the
    /// "columns missing from target" set directly without a length
    /// pre-check.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorCode::SchemaMismatch`] if any element's
    ///   `hyper_type` is not a known Hyper type (same validation as
    ///   `create_table`).
    /// - Propagates the underlying SQL error from the single ALTER
    ///   statement. Because Hyper executes a multi-column ADD
    ///   atomically, a failure leaves the table schema unchanged —
    ///   no partial widening.
    pub fn alter_table_add_columns(
        &self,
        table: &str,
        cols: &[ColumnSchema],
    ) -> Result<(), McpError> {
        self.alter_table_add_columns_in(None, table, cols)
    }

    /// Like [`Self::alter_table_add_columns`] but for a table in
    /// `target_db`. `None` keeps the unqualified identifier; `Some(alias)`
    /// emits `"db"."public"."table"` so the ALTER lands in the attached
    /// database.
    ///
    /// # Errors
    ///
    /// Same as [`Self::alter_table_add_columns`].
    pub fn alter_table_add_columns_in(
        &self,
        target_db: Option<&str>,
        table: &str,
        cols: &[ColumnSchema],
    ) -> Result<(), McpError> {
        if cols.is_empty() {
            return Ok(());
        }
        for col in cols {
            if crate::schema::map_hyper_type(&col.hyper_type).is_none() {
                return Err(McpError::new(
                    ErrorCode::SchemaMismatch,
                    format!(
                        "Unknown type '{}' for column '{}'",
                        col.hyper_type, col.name
                    ),
                ));
            }
        }
        let quoted_table = match target_db {
            Some(db) => {
                let esc_db = db.replace('"', "\"\"");
                let esc_tbl = table.replace('"', "\"\"");
                format!("\"{esc_db}\".\"public\".\"{esc_tbl}\"")
            }
            None => format!("\"{}\"", table.replace('"', "\"\"")),
        };
        let add_clauses = cols
            .iter()
            .map(|c| {
                format!(
                    "ADD COLUMN \"{}\" {}",
                    c.name.replace('"', "\"\""),
                    c.hyper_type
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("ALTER TABLE {quoted_table} {add_clauses}");
        self.connection
            .execute_command(&sql)
            .map_err(McpError::from)?;
        Ok(())
    }

    /// List all tables in the `public` schema with their column definitions
    /// and row counts. Returned as a JSON-serializable `Vec` for direct use
    /// in MCP tool responses.
    ///
    /// # Errors
    ///
    /// - Propagates any error from [`Catalog::get_table_names`] (typically
    ///   connection loss or SQL errors from the underlying catalog
    ///   probe).
    /// - Propagates any error from `describe_table_with_catalog` for
    ///   individual tables — a single failing describe aborts the whole
    ///   listing.
    pub fn describe_tables(&self) -> Result<Vec<Value>, McpError> {
        let catalog = Catalog::new(&self.connection);
        let table_names = catalog.get_table_names("public").map_err(McpError::from)?;
        let mut tables = Vec::new();
        for name in &table_names {
            // Skip infrastructure tables (`_hyperdb_*`) so the public
            // catalog only surfaces user-visible data. See
            // [`is_internal_table`] for the convention and rationale.
            if is_internal_table(name.as_str()) {
                continue;
            }
            tables.push(describe_table_with_catalog(&catalog, name.as_str())?);
        }
        Ok(tables)
    }

    /// Describe a single table by name. Returns the same JSON shape as an
    /// element of [`Self::describe_tables`] (`name`, `columns`, `row_count`).
    ///
    /// Errors with [`ErrorCode::TableNotFound`] when the table doesn't exist
    /// or is an internal `_hyperdb_*` bookkeeping table (callers should not
    /// be able to probe infrastructure via this path; it stays consistent
    /// with the full-list variant that hides them).
    ///
    /// Uses `get_table_names("public")` as the authoritative existence check
    /// rather than pattern-matching the error string from
    /// `get_table_definition`, because the latter's wording varies across
    /// Hyper versions and can slip past `translate_table_missing`.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorCode::TableNotFound`] if `table_name` is an
    ///   internal `_hyperdb_*` table or does not appear in `public`.
    /// - Propagates any error from [`Catalog::get_table_names`] or from
    ///   `describe_table_with_catalog` (connection loss, catalog probe
    ///   failures).
    pub fn describe_table(&self, table_name: &str) -> Result<Value, McpError> {
        if is_internal_table(table_name) {
            return Err(McpError::new(
                ErrorCode::TableNotFound,
                format!("Table '{table_name}' does not exist"),
            ));
        }
        let catalog = Catalog::new(&self.connection);
        let exists = catalog
            .get_table_names("public")
            .map_err(McpError::from)?
            .iter()
            .any(|n| n.as_str() == table_name);
        if !exists {
            return Err(McpError::new(
                ErrorCode::TableNotFound,
                format!("Table '{table_name}' does not exist"),
            ));
        }
        describe_table_with_catalog(&catalog, table_name)
    }

    /// Sample rows from a table along with its schema and total row count.
    ///
    /// Returns a single JSON object with `table`, `row_count`, `sample_size`,
    /// `schema`, and `rows`. `n` is clamped to the range `1..=100`.
    /// Returns [`ErrorCode::TableNotFound`] if the table doesn't exist.
    ///
    /// Avoids the `Catalog::has_table` probe entirely — we just run the sample
    /// SELECT first and translate a Hyper "table does not exist" error into
    /// our own [`ErrorCode::TableNotFound`]. This sidesteps the old pattern
    /// where a racy `has_table` silently returning `Err` would be rewritten
    /// to `false` and surface as a spurious `TableNotFound` for tables that
    /// actually exist.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorCode::TableNotFound`] (via `translate_table_missing`)
    ///   if the sample `SELECT` surfaces a Hyper "table does not exist" error.
    /// - Propagates any other [`McpError`] from the sample query — SQL
    ///   errors, permission failures, or connection loss.
    /// - The subsequent `COUNT(*)` and `get_table_definition` calls are
    ///   best-effort: their errors are swallowed so the sample payload
    ///   is still returned when available.
    pub fn sample_table(&self, table_name: &str, n: u64) -> Result<Value, McpError> {
        self.sample_table_in(None, table_name, n)
    }

    /// Sample rows from a table in `target_db` (or the primary when `None`).
    ///
    /// # Errors
    ///
    /// Same as [`Self::sample_table`].
    pub fn sample_table_in(
        &self,
        target_db: Option<&str>,
        table_name: &str,
        n: u64,
    ) -> Result<Value, McpError> {
        let n = n.clamp(1, 100);
        let qualified = match target_db {
            Some(db) => {
                let esc_db = db.replace('"', "\"\"");
                let esc_tbl = table_name.replace('"', "\"\"");
                format!("\"{esc_db}\".\"public\".\"{esc_tbl}\"")
            }
            None => format!("\"{}\"", table_name.replace('"', "\"\"")),
        };

        let select_sql = format!("SELECT * FROM {qualified} LIMIT {n}");
        let rows = match self.execute_query_to_json(&select_sql) {
            Ok(r) => r,
            Err(e) => return Err(translate_table_missing(e, table_name)),
        };

        let count_sql = format!("SELECT COUNT(*) AS cnt FROM {qualified}");
        let row_count = self
            .execute_query_to_json(&count_sql)
            .ok()
            .and_then(|rs| {
                rs.first()
                    .and_then(|r| r.get("cnt").and_then(serde_json::Value::as_i64))
            })
            .unwrap_or(0);

        // Column metadata: when targeting the primary, use the
        // connection-bound Catalog. For other databases, query
        // pg_catalog.pg_attribute directly via fully-qualified SQL.
        let columns: Vec<Value> = match target_db {
            None => {
                let catalog = Catalog::new(&self.connection);
                catalog
                    .get_table_definition(table_name)
                    .map(|def| {
                        def.columns()
                            .iter()
                            .map(|col| {
                                json!({
                                    "name": col.name,
                                    "type": col.type_name(),
                                    "nullable": col.nullable,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            Some(db) => describe_columns_via_pg_catalog(self, db, table_name).unwrap_or_default(),
        };

        Ok(json!({
            "table": table_name,
            "row_count": row_count,
            "sample_size": rows.len(),
            "schema": columns,
            "rows": rows,
        }))
    }

    /// List public tables in `target_db` (or the primary when `None`).
    ///
    /// # Errors
    ///
    /// Returns [`McpError`] on catalog query failure.
    pub fn describe_tables_in(&self, target_db: Option<&str>) -> Result<Vec<Value>, McpError> {
        match target_db {
            None => self.describe_tables(),
            Some(db) => {
                let esc_db = db.replace('"', "\"\"");
                let list_sql = format!(
                    "SELECT tablename FROM \"{esc_db}\".pg_catalog.pg_tables \
                     WHERE schemaname = 'public' ORDER BY tablename"
                );
                let names_rows = self.execute_query_to_json(&list_sql)?;
                let mut out = Vec::new();
                for row in &names_rows {
                    let Some(name) = row.get("tablename").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if is_internal_table(name) {
                        continue;
                    }
                    out.push(self.describe_table_in(Some(db), name)?);
                }
                Ok(out)
            }
        }
    }

    /// Describe a single table in `target_db` (or the primary when `None`).
    ///
    /// # Errors
    ///
    /// Same as [`Self::describe_table`].
    pub fn describe_table_in(
        &self,
        target_db: Option<&str>,
        table_name: &str,
    ) -> Result<Value, McpError> {
        if is_internal_table(table_name) {
            return Err(McpError::new(
                ErrorCode::TableNotFound,
                format!("Table '{table_name}' does not exist"),
            ));
        }
        match target_db {
            None => self.describe_table(table_name),
            Some(db) => {
                // Existence check via pg_catalog
                let esc_db = db.replace('"', "\"\"");
                let esc_tbl = table_name.replace('\'', "''");
                let exists_sql = format!(
                    "SELECT 1 FROM \"{esc_db}\".pg_catalog.pg_tables \
                     WHERE schemaname = 'public' AND tablename = '{esc_tbl}'"
                );
                let rows = self.execute_query_to_json(&exists_sql)?;
                if rows.is_empty() {
                    return Err(McpError::new(
                        ErrorCode::TableNotFound,
                        format!("Table '{table_name}' does not exist in database '{db}'"),
                    ));
                }
                // Columns via pg_catalog.pg_attribute
                let columns = describe_columns_via_pg_catalog(self, db, table_name)?;
                // Row count
                let qualified = format!(
                    "\"{esc_db}\".\"public\".\"{}\"",
                    table_name.replace('"', "\"\"")
                );
                let count_sql = format!("SELECT COUNT(*) AS cnt FROM {qualified}");
                let row_count = self
                    .execute_query_to_json(&count_sql)
                    .ok()
                    .and_then(|rs| {
                        rs.first()
                            .and_then(|r| r.get("cnt").and_then(serde_json::Value::as_i64))
                    })
                    .unwrap_or(0);
                Ok(json!({
                    "name": table_name,
                    "row_count": row_count,
                    "columns": columns,
                }))
            }
        }
    }

    /// Collect workspace health and size metrics for the `status` MCP tool.
    ///
    /// Includes `logs` with paths to the `hyperd` log file (if one exists yet)
    /// and the MCP client log. These are the first files to check when
    /// something misbehaves.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Catalog::get_table_names`]. Per-table
    /// row counts and disk usage fall back to `0` on read failure, so
    /// these do not bubble up.
    pub fn status(&self) -> Result<Value, McpError> {
        let catalog = Catalog::new(&self.connection);
        let all_names = catalog.get_table_names("public").map_err(McpError::from)?;
        // Same filter as `describe_tables`: the saved-queries meta-table
        // and any other `_hyperdb_*` internal tables shouldn't bump the
        // user-visible `table_count` / `total_rows`.
        let table_names: Vec<_> = all_names
            .iter()
            .filter(|n| !is_internal_table(n.as_str()))
            .collect();
        let table_count = table_names.len();

        let total_rows: i64 = table_names
            .iter()
            .map(|name| catalog.get_row_count(name.as_str()).unwrap_or(0))
            .sum();

        // Disk size of the ephemeral primary. The persistent file is
        // reported separately when present.
        let ephemeral_bytes = std::fs::metadata(&self.ephemeral_path).map_or(0, |m| m.len());
        let persistent_bytes = self
            .persistent_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0u64, |m| m.len());
        let disk_bytes = ephemeral_bytes.saturating_add(persistent_bytes);

        let hyperd_log = self.hyperd_log_path().map_or(Value::Null, |p| {
            Value::String(p.to_string_lossy().into_owned())
        });
        let client_log_path = self.log_dir.join(CLIENT_LOG_FILE_NAME);
        let client_log = if client_log_path.exists() {
            Value::String(client_log_path.to_string_lossy().into_owned())
        } else {
            Value::Null
        };

        let persistent_path_value = self.persistent_path.as_ref().map_or(Value::Null, |p| {
            Value::String(p.to_string_lossy().into_owned())
        });

        Ok(json!({
            "hyperd_running": self.is_running(),
            "ephemeral_path": self.ephemeral_path.to_string_lossy(),
            "persistent_path": persistent_path_value,
            "has_persistent": self.has_persistent(),
            "table_count": table_count,
            "total_rows": total_rows,
            "disk_usage_bytes": disk_bytes,
            // The MCP server and the `hyperdb-api` crate it's built on live in
            // the same Cargo workspace and ship from the same commit, so a
            // single version string identifies both. Label it by the
            // underlying library since that's the more fundamental
            // identifier — the MCP server is a thin layer over the Hyper
            // Rust API.
            "hyper_rust_api_version": crate::version::hyper_api_version_string(),
            "logs": {
                "log_dir": self.log_dir.to_string_lossy(),
                "hyperd_log": hyperd_log,
                "client_log": client_log,
            },
        }))
    }
}

/// Convert a single cell from a Hyper result row into a JSON `Value`.
///
/// Dispatches on the column's SQL OID so each type is decoded through the
/// right [`hyperdb_api::Row::get`] instantiation. When a type isn't explicitly
/// handled, falls back to string decoding — safe for textual types but
/// produces garbage for binary types, so every type we might actually see
/// should have its own branch.
///
/// # Type mapping
///
/// | Hyper OID | JSON shape |
/// |-----------|------------|
/// | `BOOL` | `true`/`false` |
/// | `SMALL_INT` / `INT` / `BIG_INT` | number |
/// | `DOUBLE` / `FLOAT` | number |
/// | `NUMERIC` | number when losslessly representable as `f64`, else string |
/// | `DATE` | ISO 8601 date string (`YYYY-MM-DD`) |
/// | `TIMESTAMP` / `TIMESTAMP_TZ` | ISO 8601 timestamp string |
/// | `TEXT` / `VARCHAR` | string |
/// | anything else | string (fallback; may be garbage for binary types) |
fn row_value_to_json(row: &hyperdb_api::Row, idx: usize, sql_type: &SqlType) -> Value {
    use hyperdb_api::oids;
    use hyperdb_api::{Date, Numeric, OffsetTimestamp, Timestamp};

    if row.is_null(idx) {
        return Value::Null;
    }
    let oid_val = sql_type.internal_oid();
    if oid_val == oids::BOOL.0 {
        return row.get::<bool>(idx).map_or(Value::Null, Value::Bool);
    }
    if oid_val == oids::SMALL_INT.0 {
        return row
            .get::<i16>(idx)
            .map_or(Value::Null, |v| Value::Number(v.into()));
    }
    if oid_val == oids::INT.0 {
        return row
            .get::<i32>(idx)
            .map_or(Value::Null, |v| Value::Number(v.into()));
    }
    if oid_val == oids::BIG_INT.0 {
        return row
            .get::<i64>(idx)
            .map_or(Value::Null, |v| Value::Number(v.into()));
    }
    if oid_val == oids::DOUBLE.0 || oid_val == oids::FLOAT.0 {
        return row
            .get::<f64>(idx)
            .and_then(|v| serde_json::Number::from_f64(v).map(Value::Number))
            .unwrap_or(Value::Null);
    }
    if oid_val == oids::NUMERIC.0 {
        // `Row` is schema-aware as of the upstream NUMERIC fix — it
        // carries an `Arc<ResultSchema>` and `row.get::<Numeric>()`
        // reads the scale from the column's
        // `SqlType::Numeric { precision, scale }` descriptor before
        // dispatching on the buffer length. That covers all three
        // NUMERIC wire shapes the server can send on a query result:
        //
        //   * 8-byte  `Numeric`     (precision ≤ 18, e.g. `AVG(INT)`)
        //   * 16-byte `BigNumeric`  (precision > 18)
        //   * Arrow `Decimal128`/`Decimal256` (gRPC transport)
        //
        // Prior to the upstream fix, `type_modifier` was being dropped
        // during `RowDescription` parsing so the scale presented here
        // was always `0`, the 8-byte form wasn't decodable at all, and
        // `AVG` results fell through to `Null`. All of that is now
        // handled inside `hyperdb-api`; this function only needs to pick
        // the JSON shape.
        //
        // `Numeric::to_string()` uses the decoded scale, so round-trip
        // through `f64` is only used for JSON compactness — if the
        // value doesn't fit in `f64` losslessly (`serde_json::Number::
        // from_f64` returns `None` for NaN/Infinity, and we can't
        // always represent large i128 exactly as `f64`), fall back to
        // the string form so the caller sees the exact value.
        return row.get::<Numeric>(idx).map_or(Value::Null, |n| {
            let s = n.to_string();
            s.parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .unwrap_or(Value::String(s))
        });
    }
    if oid_val == oids::DATE.0 {
        // `Date`'s `Display` impl already formats as ISO 8601 `YYYY-MM-DD`.
        return row
            .get::<Date>(idx)
            .map_or(Value::Null, |d| Value::String(d.to_string()));
    }
    if oid_val == oids::TIMESTAMP.0 {
        return row
            .get::<Timestamp>(idx)
            .map_or(Value::Null, |t| Value::String(t.to_string()));
    }
    if oid_val == oids::TIMESTAMP_TZ.0 {
        return row
            .get::<OffsetTimestamp>(idx)
            .map_or(Value::Null, |t| Value::String(t.to_string()));
    }
    if oid_val == oids::TEXT.0 || oid_val == oids::VARCHAR.0 {
        return row.get::<String>(idx).map_or(Value::Null, Value::String);
    }
    // Fallback: try as string. Safe for textual types we didn't list;
    // produces garbage bytes for binary types (BYTEA, GEOGRAPHY, …)
    // — add explicit branches above when those start appearing in
    // real queries.
    row.get::<String>(idx).map_or(Value::Null, Value::String)
}

/// Name of the client-side log file written in [`resolve_log_dir`].
/// The MCP binary's `main` opens this file and sets it as a `tracing`
/// subscriber target so both startup errors and runtime events land here.
pub const CLIENT_LOG_FILE_NAME: &str = "hyperdb-mcp.log";

/// Name-prefix convention for tables that belong to the `HyperDB` MCP's
/// own infrastructure (currently the `_hyperdb_saved_queries` meta-table
/// used by `WorkspaceStore`). Hidden from [`Engine::describe_tables`]
/// and from [`Engine::status`]'s `table_count` / `total_rows`, so users
/// never see `HyperDB`'s own bookkeeping in the public catalog.
///
/// Any future internal table (watcher state, audit log, etc.) just
/// needs to follow this prefix and it disappears from the public view
/// automatically — no per-table filter list to keep in sync.
pub const HYPERDB_INTERNAL_PREFIX: &str = "_hyperdb_";

/// Returns true when `name` is one of `HyperDB`'s own internal tables
/// (matches [`HYPERDB_INTERNAL_PREFIX`]). Factored into a helper so
/// every filter site calls the same predicate and a future move to a
/// more nuanced scheme (e.g. per-table allowlist) is a single edit.
///
/// Note: `_table_catalog` lives in the persistent attachment, not the
/// ephemeral primary, so it doesn't show up in `describe_tables` even
/// without the filter — `describe_tables` only enumerates the primary.
#[must_use]
pub fn is_internal_table(name: &str) -> bool {
    name.starts_with(HYPERDB_INTERNAL_PREFIX)
}

/// Compute the log directory for both `hyperd` output and the client-side
/// tracing log. Shared by [`Engine::new`] and `main` so both land in the
/// same place.
///
/// - When a persistent path is supplied: same directory as that file
///   (with `~` expansion applied). A project DB like
///   `~/projects/foo.hyper` gets logs in `~/projects/`.
/// - When no persistent path is supplied (ephemeral-only sessions):
///   `$TMPDIR/hyperdb-mcp-<pid>/`. Multiple engines in the same PID
///   share this log dir, which is fine — `tracing` is process-wide and
///   the `.hyper` files themselves live in distinct per-engine subdirs.
#[must_use]
pub fn resolve_log_dir(persistent_db_path: Option<&str>) -> PathBuf {
    match persistent_db_path {
        Some(p) => {
            let expanded = PathBuf::from(shellexpand_tilde(p));
            expanded
                .parent()
                .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf)
        }
        None => std::env::temp_dir().join(format!("hyperdb-mcp-{}", std::process::id())),
    }
}

/// Build the `{name, columns, row_count}` JSON for a single table, shared
/// between [`Engine::describe_tables`] (bulk) and [`Engine::describe_table`]
/// (single) so both paths emit byte-identical shapes. A missing table
/// surfaces as the underlying Hyper "relation does not exist" error; single-
/// table callers should run it through `translate_table_missing`.
/// Describe columns of `table_name` in attached database `db_alias` by
/// querying that database's `pg_catalog.pg_attribute` directly. Used when
/// the connection-bound `Catalog` API can't see the target database.
fn describe_columns_via_pg_catalog(
    engine: &Engine,
    db_alias: &str,
    table_name: &str,
) -> Result<Vec<Value>, McpError> {
    let esc_db = db_alias.replace('"', "\"\"");
    let esc_tbl = table_name.replace('\'', "''");
    let sql = format!(
        "SELECT a.attname AS name, \
                t.typname AS type_name, \
                NOT a.attnotnull AS nullable, \
                a.attnum AS ordinal \
         FROM \"{esc_db}\".pg_catalog.pg_attribute a \
         JOIN \"{esc_db}\".pg_catalog.pg_class c ON a.attrelid = c.oid \
         JOIN \"{esc_db}\".pg_catalog.pg_namespace n ON c.relnamespace = n.oid \
         JOIN \"{esc_db}\".pg_catalog.pg_type t ON a.atttypid = t.oid \
         WHERE n.nspname = 'public' \
           AND c.relname = '{esc_tbl}' \
           AND a.attnum > 0 \
         ORDER BY a.attnum"
    );
    let rows = engine.execute_query_to_json(&sql)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            json!({
                "name": r.get("name").cloned().unwrap_or(Value::Null),
                "type": r.get("type_name").cloned().unwrap_or(Value::Null),
                "nullable": r.get("nullable").cloned().unwrap_or(Value::Bool(true)),
            })
        })
        .collect())
}

fn describe_table_with_catalog(catalog: &Catalog<'_>, name: &str) -> Result<Value, McpError> {
    let def = catalog.get_table_definition(name).map_err(McpError::from)?;
    let row_count = catalog.get_row_count(name).unwrap_or(0);
    let columns: Vec<Value> = def
        .columns()
        .iter()
        .map(|col| {
            json!({
                "name": col.name,
                "type": col.type_name(),
                "nullable": col.nullable,
            })
        })
        .collect();
    Ok(json!({
        "name": name,
        "columns": columns,
        "row_count": row_count,
    }))
}

/// Translate an "undefined table / relation does not exist" error from Hyper
/// into our own [`ErrorCode::TableNotFound`] with a consistent message.
/// Any other error is passed through unchanged.
fn translate_table_missing(err: McpError, table_name: &str) -> McpError {
    let m = err.message.to_lowercase();
    let looks_like_missing = m.contains("does not exist")
        || m.contains("relation")
        || m.contains("undefined table")
        || err.message.contains("42P01");
    if looks_like_missing {
        McpError::new(
            ErrorCode::TableNotFound,
            format!("Table '{table_name}' does not exist"),
        )
    } else {
        err
    }
}

/// Returns `true` if a SQL statement is read-only: `SELECT`, `WITH`, `EXPLAIN`,
/// `SHOW`, or `VALUES`. Anything else (`CREATE`, `INSERT`, `UPDATE`, `DELETE`,
/// `DROP`, `ALTER`, `COPY`, ...) is considered mutating.
///
/// The check is a simple prefix match after trimming and upper-casing the first
/// Checks whether the first SQL keyword indicates a read-only statement.
///
/// Strips leading whitespace and SQL comments (line `--` and block `/* */`)
/// before inspecting the first alphabetic token. This prevents comment-based
/// bypass of the read-only guard (e.g. `/* harmless */ DROP TABLE ...`).
///
/// Note: data-modifying CTEs (`WITH x AS (DELETE ...) SELECT ...`) still slip
/// past this check. Hyper itself rejects such CTEs, so this is defense-in-depth
/// rather than the sole security boundary.
#[must_use]
pub fn is_read_only_sql(sql: &str) -> bool {
    let stripped = strip_leading_sql_comments(sql);
    let first_token: String = stripped
        .chars()
        .take_while(|c| c.is_alphabetic())
        .flat_map(char::to_uppercase)
        .collect();
    matches!(
        first_token.as_str(),
        "SELECT" | "WITH" | "EXPLAIN" | "SHOW" | "VALUES"
    )
}

/// Strips leading whitespace, line comments (`--`), and block comments (`/* */`)
/// from SQL text. Handles nested block comments.
fn strip_leading_sql_comments(sql: &str) -> &str {
    let mut s = sql;
    loop {
        s = s.trim_start();
        if s.starts_with("--") {
            // Line comment — skip to end of line (handles LF, CRLF, and CR)
            match s.find(&['\n', '\r'][..]) {
                Some(pos) => {
                    let mut next = pos + 1;
                    // Handle CRLF: skip both characters
                    if s.as_bytes().get(pos) == Some(&b'\r')
                        && s.as_bytes().get(pos + 1) == Some(&b'\n')
                    {
                        next = pos + 2;
                    }
                    s = &s[next..];
                }
                None => return "",
            }
        } else if s.starts_with("/*") {
            // Block comment — find matching close, handling nesting
            let mut depth = 0u32;
            let mut chars = s.char_indices().peekable();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                if c == '/' && chars.peek().map(|(_, c2)| *c2) == Some('*') {
                    chars.next();
                    depth += 1;
                } else if c == '*' && chars.peek().map(|(_, c2)| *c2) == Some('/') {
                    chars.next();
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 2);
                        break;
                    }
                }
            }
            match end {
                Some(pos) => s = &s[pos..],
                None => return "", // Unclosed comment — no valid SQL
            }
        } else {
            break;
        }
    }
    s
}

impl Drop for Engine {
    fn drop(&mut self) {
        // The ephemeral primary is always cleaned up. In daemon mode the
        // shared hyperd holds the file handle even after this engine is
        // dropped, so we DETACH first (Windows enforces file locks; this
        // is a no-op on Unix but keeps behavior identical across platforms).
        // The persistent attachment is left in place — its lifetime
        // outlives the engine.
        if self.daemon_endpoint.is_some() {
            let db_name = self.primary_db_name();
            let detach = format!("DETACH DATABASE \"{db_name}\"");
            let _ = self.connection.execute_command(&detach);
        }
        // Remove the per-pid temp directory holding the ephemeral file.
        // Safe in both daemon and local modes: in local mode the
        // HyperProcess Drop tears down hyperd before this fires (Drop
        // runs in field-declaration order), so the file is no longer
        // open by the time we delete it.
        if let Some(parent) = self.ephemeral_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

fn bootstrap_public_schema(connection: &Connection) -> Result<(), McpError> {
    connection
        .execute_command("CREATE SCHEMA IF NOT EXISTS public")
        .map(|_| ())
        .map_err(|e| {
            McpError::new(
                ErrorCode::InternalError,
                format!("Failed to bootstrap public schema: {e}"),
            )
        })
}

/// Minimal `~/` (and `~\` on Windows) expansion. Resolves the home
/// directory via `$HOME` on Unix and `%USERPROFILE%` (falling back to
/// `%HOMEDRIVE%%HOMEPATH%`) on Windows. `~username/` is not supported —
/// callers who need that should expand their paths themselves.
fn shellexpand_tilde(path: &str) -> String {
    let rest = if let Some(r) = path.strip_prefix("~/") {
        Some(r)
    } else if cfg!(windows) {
        path.strip_prefix("~\\")
    } else {
        None
    };
    let Some(rest) = rest else {
        return path.to_string();
    };
    let Some(home) = home_dir() else {
        return path.to_string();
    };
    let sep = std::path::MAIN_SEPARATOR;
    format!("{}{sep}{rest}", home.to_string_lossy())
}

/// Resolve the user's home directory across platforms. Unix uses `$HOME`;
/// Windows prefers `%USERPROFILE%` and falls back to `%HOMEDRIVE%%HOMEPATH%`.
fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            if !profile.is_empty() {
                return Some(PathBuf::from(profile));
            }
        }
        let drive = std::env::var_os("HOMEDRIVE")?;
        let rel = std::env::var_os("HOMEPATH")?;
        let mut combined = PathBuf::from(drive);
        combined.push(PathBuf::from(rel));
        Some(combined)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}
