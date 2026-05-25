// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Registry of attached databases for cross-database queries.
//!
//! The primary workspace opened by [`crate::engine::Engine`] is always
//! addressable under the reserved alias `"local"`. Callers can attach
//! additional `.hyper` files under user-chosen aliases via
//! [`AttachRegistry::attach`]; the registry tracks every live attachment
//! so it can be *replayed* after an [`crate::error::ErrorCode::ConnectionLost`]
//! auto-reconnect rebuilds the underlying Hyper connection.
//!
//! # Future kinds
//!
//! [`AttachSource`] is a tagged enum so future remote kinds (TCP to a
//! standard `hyperd`, gRPC to a Data 360 Hyper) plug in without breaking
//! the registry API or the MCP tool schemas. Only [`AttachSource::LocalFile`]
//! is implemented today; the MCP tool layer rejects other `kind` values
//! with a clear "not yet supported" message.
//!
//! # Safety model
//!
//! - **Path policy.** `LocalFile` paths must be absolute and
//!   canonicalized (`..` components rejected) so the LLM cannot traverse
//!   outside the filesystem root via relative tricks.
//! - **Alias policy.** Aliases are validated as strict SQL identifiers
//!   (`[A-Za-z_][A-Za-z0-9_]{0,62}`) and cannot collide with `"local"`.
//! - **Read-only posture.** Attachments default to read-only. Writable
//!   mode is opt-in and is still subject to the server-level `--read-only`
//!   guard — `--read-only` always wins.

use crate::engine::Engine;
use crate::error::{ErrorCode, McpError};
use hyperdb_api::escape_sql_path;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

/// Alias reserved for the server's primary workspace. Users cannot
/// attach under this name; `copy_query` treats `target_database: "local"`
/// the same as the unqualified default.
pub const LOCAL_ALIAS: &str = "local";

/// Escape `s` as a single-quoted SQL string literal (ANSI: double the
/// embedded single quotes, nothing else is special). Used for
/// `SET schema_search_path = '…'`.
fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Install the primary workspace as the `schema_search_path` so that
/// unqualified name resolution keeps routing into the primary after
/// one or more `ATTACH DATABASE`s have run. Hyper's out-of-the-box
/// default is `"$single"`, which only works while the connection
/// has exactly one database — the moment a second is attached,
/// `"$single"` resolves to nothing and unqualified references start
/// raising `relation does not exist`. See `docs/attach_search_path.md`
/// (if added) or the bug-fix PR description for the investigation
/// trail.
fn set_primary_search_path(engine: &Engine) -> Result<(), McpError> {
    let sql = format!(
        "SET schema_search_path = {}",
        sql_string_literal(&engine.primary_db_name()),
    );
    engine.execute_command(&sql)?;
    Ok(())
}

/// Restore the connection's search-path posture when the user-visible
/// attachment registry transitions back to zero attachments.
///
/// - When the engine has the default persistent attachment, leave the
///   pin in place: even with no user attachments, the persistent DB is
///   *still attached*, so Hyper's `"$single"` resolution would fail.
///   We pin explicitly to the ephemeral primary's name.
/// - When `--ephemeral-only` (no persistent attachment), restore Hyper's
///   default `"$single"` mode so the connection behaves exactly like a
///   fresh single-database session.
fn reset_search_path(engine: &Engine) -> Result<(), McpError> {
    if engine.has_persistent() {
        // Re-pin to the primary's name so unqualified resolution keeps
        // working alongside the ever-present persistent attachment.
        set_primary_search_path(engine)
    } else {
        engine.execute_command("RESET schema_search_path")?;
        Ok(())
    }
}

/// Policy for what [`AttachRegistry::attach`] should do when the
/// requested `LocalFile` path does not exist. Applies only to the
/// `local_file` kind today; remote kinds (`tcp`, `grpc`) will ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnMissing {
    /// Return [`ErrorCode::FileNotFound`]. Default; matches the pre-
    /// existing behavior.
    #[default]
    Error,
    /// Create an empty `.hyper` file at the target path first, then
    /// attach it. Requires `writable: true` — an empty database that
    /// the session cannot mutate has no use.
    Create,
}

impl OnMissing {
    /// Parse the MCP tool parameter. `None` and the empty string map to
    /// [`OnMissing::Error`] so callers can omit the field.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidArgument`] when `value` is a non-empty
    /// string other than `"error"` or `"create"`.
    pub fn parse(value: Option<&str>) -> Result<Self, McpError> {
        match value.map(str::trim) {
            None | Some("" | "error") => Ok(Self::Error),
            Some("create") => Ok(Self::Create),
            Some(other) => Err(McpError::new(
                ErrorCode::InvalidArgument,
                format!("on_missing must be 'error' or 'create', got '{other}'"),
            )),
        }
    }
}

/// Where an attached database lives. Kind-tagged so future remote
/// variants (TCP, gRPC) can slot in without breaking the registry API
/// or MCP tool schemas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachSource {
    /// A `.hyper` file on the local filesystem. Paths are absolute and
    /// canonicalized before landing in the registry.
    LocalFile {
        /// Canonical absolute path to the `.hyper` file.
        path: PathBuf,
    },
    // Future: Tcp  { endpoint: String, auth: Option<TcpAuth> },
    // Future: Grpc { endpoint: String, auth: Option<GrpcAuth> }, // writable always false
}

impl AttachSource {
    /// Machine-readable kind tag used in MCP tool params and responses.
    #[must_use]
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::LocalFile { .. } => "local_file",
        }
    }

    /// JSON shape for `list_attached_databases` / `status`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::LocalFile { path } => json!({
                "kind": "local_file",
                "path": path.to_string_lossy(),
            }),
        }
    }
}

/// One live attachment. Constructed by [`AttachRegistry::attach`] and
/// returned unchanged until the alias is detached.
#[derive(Debug, Clone)]
pub struct AttachedDb {
    pub alias: String,
    pub source: AttachSource,
    pub writable: bool,
    pub attached_at: SystemTime,
}

impl AttachedDb {
    /// JSON shape for `list_attached_databases` / `status`. Timestamp
    /// is emitted as RFC 3339 so clients don't need to know the
    /// internal format.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let attached_at = chrono::DateTime::<chrono::Utc>::from(self.attached_at).to_rfc3339();
        json!({
            "alias": self.alias,
            "source": self.source.to_json(),
            "kind": self.source.kind_str(),
            "writable": self.writable,
            "attached_at": attached_at,
        })
    }
}

/// Request shape for [`AttachRegistry::attach`]. Pre-validated by the
/// MCP tool layer; the registry re-validates defensively because it is
/// also the entry point for replay.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    pub alias: String,
    pub source: AttachSource,
    pub writable: bool,
    /// What to do when `source` points at a `.hyper` file that does
    /// not yet exist. [`OnMissing::Error`] (the default) preserves the
    /// original "file must already exist" contract; [`OnMissing::Create`]
    /// asks the registry to issue `CREATE DATABASE IF NOT EXISTS` before
    /// attaching, which requires `writable: true`.
    pub on_missing: OnMissing,
}

/// Live set of attachments keyed by alias. Thread-safe via an internal
/// `Mutex`; all operations are serial, which matches the rest of the
/// engine's single-connection model.
///
/// The registry holds *user-attached* databases — the default persistent
/// database is attached directly by [`crate::engine::Engine`] and isn't
/// tracked here. Replay-on-reconnect only re-issues the user attaches;
/// the engine re-attaches persistent itself when it's reconstructed.
#[derive(Debug)]
pub struct AttachRegistry {
    // Insertion-ordered so replay happens in the same order the user
    // originally attached — matters if attachment B references objects
    // that rely on attachment A (not today, but cheap to preserve).
    inner: Mutex<Vec<AttachedDb>>,
}

impl Default for AttachRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AttachRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
        }
    }

    /// Attach a database into the current engine's connection and store
    /// it in the registry. Caller is responsible for read-only
    /// enforcement (`--read-only` + `writable: true` combination).
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorCode::InvalidArgument`] if the alias fails
    ///   [`validate_alias`], if the alias is already in use, or if
    ///   `on_missing=Create` is combined with `writable=false`.
    /// - Returns [`ErrorCode::FileNotFound`] when `on_missing=Error`
    ///   and the target `.hyper` path does not exist.
    /// - Returns [`ErrorCode::InternalError`] if the registry mutex is
    ///   poisoned (bubbled up from `AttachRegistry::lock`).
    /// - Propagates any error from the underlying `ATTACH DATABASE`
    ///   (and the optional `CREATE DATABASE IF NOT EXISTS`) executed
    ///   on the engine's connection — surfaced through the `?` operator
    ///   in the body.
    pub fn attach(&self, engine: &Engine, mut req: AttachRequest) -> Result<AttachedDb, McpError> {
        validate_alias(&req.alias)?;
        // Canonicalize the alias to lowercase before storage so the
        // registry, the cache key in `Engine::catalog_present_cache`,
        // and the SQL identifier in `qualified_catalog_in` all agree.
        // Pre-canonicalization, attach was case-sensitive while the
        // cache and the persistent-alias check were case-insensitive,
        // which let `attach("User_DB")` + `detach("user_db")` silently
        // no-op while the cache stayed populated.
        req.alias = req.alias.to_ascii_lowercase();

        let mut guard = self.lock()?;
        if guard.iter().any(|a| a.alias == req.alias) {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "Alias '{}' is already in use. Detach it first or pick a different alias.",
                    req.alias
                ),
            ));
        }

        // Build the ATTACH DATABASE statement. Both the path and the
        // alias have to be safely quoted — the alias was already
        // validated to match the SQL identifier regex, but we still
        // quote it so mixed-case names survive.
        //
        // For `OnMissing::Create` we issue `CREATE DATABASE IF NOT
        // EXISTS` first so the attach step sees a valid file. The
        // create-then-attach is not a transaction — if ATTACH fails
        // right after we created the file the file stays behind, which
        // is intentional: the LLM can retry with a different alias or
        // inspect/delete the file out-of-band.
        //
        // We also latch `file_was_created` here (true iff we actually
        // ran `CREATE DATABASE` because the target file was missing).
        // The post-attach `_table_catalog` seeding step consults this
        // flag so that attaching an *existing* database — even via
        // `on_missing: create` idempotently — never mutates its
        // schema.
        let mut file_was_created = false;
        let sql = match &req.source {
            AttachSource::LocalFile { path } => {
                if !path.exists() {
                    match req.on_missing {
                        OnMissing::Error => {
                            return Err(McpError::new(
                                ErrorCode::FileNotFound,
                                format!(
                                    "Attach path does not exist: {}. \
                                     Pass on_missing='create' (with writable:true) \
                                     to create an empty .hyper file at that path.",
                                    path.display()
                                ),
                            ));
                        }
                        OnMissing::Create => {
                            if !req.writable {
                                return Err(McpError::new(
                                    ErrorCode::InvalidArgument,
                                    "on_missing='create' requires writable:true — \
                                     an empty .hyper file that cannot be written to \
                                     cannot be populated.",
                                ));
                            }
                            let create_sql = format!(
                                "CREATE DATABASE IF NOT EXISTS {}",
                                escape_sql_path(&path.to_string_lossy()),
                            );
                            engine.execute_command(&create_sql)?;
                            file_was_created = true;
                        }
                    }
                }
                format!(
                    "ATTACH DATABASE {path} AS \"{alias}\"",
                    path = escape_sql_path(&path.to_string_lossy()),
                    alias = req.alias.replace('"', "\"\""),
                )
            }
        };

        engine.execute_command(&sql)?;

        // Hyper's default `schema_search_path = "$single"` stops
        // resolving unqualified names the moment the connection has
        // more than one attached database. Pin it to the primary's
        // own name so every tool that issues unqualified SQL
        // (`describe`, `status`, `_table_catalog` upserts, …) keeps
        // routing into the primary workspace as if nothing else were
        // attached.
        //
        // If the SET fails we treat the whole attach as failed and
        // roll back with `DETACH`: succeeding here but leaving
        // `schema_search_path` unpinned puts the session into a
        // silently-broken state where unqualified local queries start
        // erroring, which is far worse than a loud up-front error.
        if let Err(e) = set_primary_search_path(engine) {
            let detach_sql = format!("DETACH DATABASE \"{}\"", req.alias.replace('"', "\"\""));
            if let Err(de) = engine.execute_command(&detach_sql) {
                tracing::warn!(
                    alias = %req.alias,
                    err = %de.message,
                    "rollback DETACH after schema_search_path failure also failed; \
                     connection is in an inconsistent state — reconnect will clear it",
                );
            }
            return Err(e);
        }

        // Seed `_table_catalog` into a freshly-created attached
        // database so opening that file as a primary workspace later
        // (on a fresh MCP instance) finds the catalog ready and skips
        // the backfill sweep. Gated on `file_was_created` only:
        // attaching an *existing* database must never mutate its
        // schema, regardless of contents. (`--bare` used to add a
        // second gate via `seed_catalog_on_create`; that flag was
        // removed when `--bare` was retired in favor of the uniform
        // "always seed on create" policy.)
        //
        // On failure we roll back the attach to preserve the
        // all-or-nothing contract: the user asked for "create a new
        // DB" which implicitly promises a catalog; leaving an
        // attached-but-unseeded file would silently violate that.
        if file_was_created {
            if let Err(e) = crate::table_catalog::ensure_exists_in(engine, Some(&req.alias)) {
                let detach_sql = format!("DETACH DATABASE \"{}\"", req.alias.replace('"', "\"\""));
                if let Err(de) = engine.execute_command(&detach_sql) {
                    tracing::warn!(
                        alias = %req.alias,
                        err = %de.message,
                        "rollback DETACH after _table_catalog seed failure also failed; \
                         alias may remain attached until reconnect",
                    );
                }
                // Also reset search_path if this was the first
                // attachment — the SET we just ran is no longer
                // backed by an attachment.
                if guard.is_empty() {
                    let _ = reset_search_path(engine);
                }
                return Err(e);
            }
        }

        let entry = AttachedDb {
            alias: req.alias,
            source: req.source,
            writable: req.writable,
            attached_at: SystemTime::now(),
        };
        guard.push(entry.clone());
        Ok(entry)
    }

    /// Detach the alias from the current connection and drop it from
    /// the registry. Returns `Ok(false)` if the alias was not present.
    ///
    /// When the detachment leaves the registry empty, restores the
    /// connection's default `schema_search_path` so unqualified name
    /// resolution returns to the single-database mode Hyper uses on a
    /// fresh connection.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorCode::InternalError`] if the registry mutex is
    ///   poisoned.
    /// - Propagates any error from the `DETACH DATABASE` statement
    ///   executed via `engine.execute_command`. A failure to reset the
    ///   `schema_search_path` afterwards is logged but NOT surfaced as
    ///   an error — the detach itself already succeeded.
    pub fn detach(&self, engine: &Engine, alias: &str) -> Result<bool, McpError> {
        // Aliases are stored lowercased (see `attach`); accept any case
        // from the caller and canonicalize before lookup.
        let alias = alias.to_ascii_lowercase();
        let mut guard = self.lock()?;
        let pos = guard.iter().position(|a| a.alias == alias);
        let Some(pos) = pos else {
            return Ok(false);
        };
        let sql = format!("DETACH DATABASE \"{}\"", alias.replace('"', "\"\""));
        engine.execute_command(&sql)?;
        guard.remove(pos);

        // Back to the fresh-connection posture: let `"$single"` take
        // over again so we don't leave a stale SET hanging around
        // that might shadow the primary's real name (for instance if
        // the user renames the workspace file across sessions).
        if guard.is_empty() {
            if let Err(e) = reset_search_path(engine) {
                tracing::warn!(
                    err = %e.message,
                    "detach succeeded but could not reset schema_search_path; \
                     unqualified queries should still work against the primary",
                );
            }
        }
        Ok(true)
    }

    /// Read-only snapshot of the current registry. Order matches the
    /// insertion order of still-live entries.
    pub fn list(&self) -> Vec<AttachedDb> {
        self.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Lookup by alias (case-insensitive). `None` if absent.
    ///
    /// Aliases are stored lowercased (see [`AttachRegistry::attach`]),
    /// so any caller supplying a mixed-case alias still finds the
    /// stored entry.
    pub fn get(&self, alias: &str) -> Option<AttachedDb> {
        let alias = alias.to_ascii_lowercase();
        self.lock()
            .ok()
            .and_then(|g| g.iter().find(|a| a.alias == alias).cloned())
    }

    /// Re-issue `ATTACH DATABASE` for every tracked entry. Used after
    /// [`crate::server::HyperMcpServer`]'s `with_engine` rebuilds a
    /// fresh [`Engine`] following a `ConnectionLost` error.
    ///
    /// Attachments that fail to replay (file moved, corrupted, held by
    /// another process) are dropped from the registry with a WARN log
    /// so the rest of the session can continue — a single stale entry
    /// should not poison the whole reconnect path.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InternalError`] if the registry mutex is
    /// poisoned. Per-entry replay failures are logged and swallowed —
    /// the method only returns `Err` for errors that prevent it from
    /// running at all.
    pub fn replay_all(&self, engine: &Engine) -> Result<(), McpError> {
        let mut guard = self.lock()?;
        let snapshot = guard.clone();
        guard.clear();

        for entry in snapshot {
            let sql = match &entry.source {
                AttachSource::LocalFile { path } => format!(
                    "ATTACH DATABASE {path} AS \"{alias}\"",
                    path = escape_sql_path(&path.to_string_lossy()),
                    alias = entry.alias.replace('"', "\"\""),
                ),
            };
            match engine.execute_command(&sql) {
                Ok(_) => guard.push(entry),
                Err(e) => {
                    tracing::warn!(
                        alias = %entry.alias,
                        err = %e.message,
                        "dropping attachment that failed to replay after reconnect",
                    );
                }
            }
        }

        // Re-pin the search path if at least one attachment survived
        // the replay. The post-ConnectionLost engine is brand-new so
        // any previous `SET schema_search_path` is gone.
        if !guard.is_empty() {
            if let Err(e) = set_primary_search_path(engine) {
                tracing::warn!(
                    err = %e.message,
                    "replay_all: could not re-pin schema_search_path after reconnect",
                );
            }
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Vec<AttachedDb>>, McpError> {
        self.inner
            .lock()
            .map_err(|_| McpError::new(ErrorCode::InternalError, "AttachRegistry lock poisoned"))
    }
}

// --- Validators -------------------------------------------------------------

/// Validate a user-supplied alias. Must match `[A-Za-z_][A-Za-z0-9_]{0,62}`
/// and must not equal [`LOCAL_ALIAS`]. The 63-char cap matches the
/// `PostgreSQL` identifier limit Hyper inherits.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidArgument`] when:
/// - `alias` equals [`LOCAL_ALIAS`] (case-insensitive).
/// - `alias` is empty or longer than 63 characters.
/// - The first character is neither an ASCII letter nor an underscore.
/// - Any subsequent character is outside `[A-Za-z0-9_]`.
///
/// Validation does NOT lowercase the alias — error messages preserve
/// the user-typed casing. [`AttachRegistry::attach`] canonicalizes the
/// alias to lowercase before storing it, so all downstream lookups
/// (registry, catalog presence cache, qualified SQL identifier) agree
/// on a single form.
///
/// # Panics
///
/// Does not panic in practice. The `chars.next().unwrap()` is guarded by
/// the preceding empty-string check, so at least one character is
/// guaranteed to exist.
pub fn validate_alias(alias: &str) -> Result<(), McpError> {
    if alias.eq_ignore_ascii_case(LOCAL_ALIAS) {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!(
                "'{LOCAL_ALIAS}' is reserved for the primary workspace and cannot be used as an attach alias."
            ),
        ));
    }
    if alias.is_empty() || alias.len() > 63 {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            "Alias must be 1..=63 characters",
        ));
    }
    let mut chars = alias.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!("Alias '{alias}' must start with a letter or underscore"),
        ));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "Alias '{alias}' contains invalid character '{c}'. \
                     Allowed: [A-Za-z_][A-Za-z0-9_]{{0,62}}"
                ),
            ));
        }
    }
    Ok(())
}

/// Validate a `LocalFile` path for the create-if-missing code path.
///
/// Looser than [`validate_local_path`] in one respect only: the target
/// file itself need not exist yet. Everything else — absolute path,
/// parent must exist, no `..` components after canonicalization — is
/// enforced identically. Delegates to [`validate_local_path`] when the
/// file is already present so the two paths produce the same canonical
/// output.
///
/// # Errors
///
/// - Returns [`ErrorCode::InvalidArgument`] if `path` is relative, has
///   no parent directory, has no file-name component, or if the
///   canonicalized parent contains `..` components.
/// - Returns [`ErrorCode::FileNotFound`] if the parent directory does
///   not exist (canonicalization fails).
/// - Delegates to [`validate_local_path`] when the file already exists,
///   producing the same errors as that function.
pub fn validate_local_path_for_create(path: &str) -> Result<PathBuf, McpError> {
    let pb = PathBuf::from(path);
    if !pb.is_absolute() {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!(
                "Attach path '{path}' must be absolute. \
                 Pass a full path to a .hyper file."
            ),
        ));
    }
    if pb.exists() {
        return validate_local_path(path);
    }
    let parent = pb.parent().ok_or_else(|| {
        McpError::new(
            ErrorCode::InvalidArgument,
            format!("Attach path '{path}' has no parent directory"),
        )
    })?;
    let file_name = pb.file_name().ok_or_else(|| {
        McpError::new(
            ErrorCode::InvalidArgument,
            format!("Attach path '{path}' has no file-name component"),
        )
    })?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|e| {
        McpError::new(
            ErrorCode::FileNotFound,
            format!(
                "Parent directory of attach path '{path}' does not exist: {e}. \
                 Create the directory first or use on_missing='error'."
            ),
        )
    })?;
    if canonical_parent
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!("Attach path '{path}' resolves to a location containing '..' components"),
        ));
    }
    Ok(canonical_parent.join(file_name))
}

/// Validate a user-supplied file path that must already exist.
///
/// Must be absolute, must exist, must canonicalize cleanly with no `..`
/// components in the result. Returns the canonical path on success.
///
/// `kind` is a short label used in error messages (e.g. `"data file"`,
/// `"export"`, `"chart output"`). For attach paths use [`validate_local_path`]
/// which uses `"attach path"` as the label.
///
/// # Errors
///
/// - Returns [`ErrorCode::InvalidArgument`] if `path` is relative or if the
///   canonicalized path contains `..` components.
/// - Returns [`ErrorCode::FileNotFound`] if `std::fs::canonicalize` fails.
pub fn validate_input_path(path: &str, kind: &str) -> Result<PathBuf, McpError> {
    let pb = PathBuf::from(path);
    if !pb.is_absolute() {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!("{kind} path '{path}' must be absolute"),
        ));
    }
    let canonical = std::fs::canonicalize(&pb).map_err(|e| {
        McpError::new(
            ErrorCode::FileNotFound,
            format!("Cannot resolve {kind} path '{path}': {e}"),
        )
    })?;
    if canonical
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!("{kind} path '{path}' resolves to a location containing '..' components"),
        ));
    }
    Ok(canonical)
}

/// Validate a user-supplied output path that may not yet exist.
///
/// Must be absolute. If the file exists, behaves like [`validate_input_path`].
/// Otherwise the parent directory must exist and canonicalize cleanly.
///
/// # Errors
///
/// Same shape as [`validate_input_path`]; additionally returns
/// [`ErrorCode::InvalidArgument`] if the path has no parent or no file-name.
pub fn validate_output_path(path: &str, kind: &str) -> Result<PathBuf, McpError> {
    let pb = PathBuf::from(path);
    if !pb.is_absolute() {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!("{kind} path '{path}' must be absolute"),
        ));
    }
    if pb.exists() {
        return validate_input_path(path, kind);
    }
    let parent = pb.parent().ok_or_else(|| {
        McpError::new(
            ErrorCode::InvalidArgument,
            format!("{kind} path '{path}' has no parent directory"),
        )
    })?;
    let file_name = pb.file_name().ok_or_else(|| {
        McpError::new(
            ErrorCode::InvalidArgument,
            format!("{kind} path '{path}' has no file-name component"),
        )
    })?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|e| {
        McpError::new(
            ErrorCode::FileNotFound,
            format!("Parent directory of {kind} path '{path}' does not exist: {e}"),
        )
    })?;
    if canonical_parent
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!("{kind} path '{path}' resolves to a location containing '..' components"),
        ));
    }
    Ok(canonical_parent.join(file_name))
}

/// Validate a `LocalFile` path. Must be absolute, must exist, must
/// canonicalize cleanly with no `..` components in the result. Returns
/// the canonical path on success.
///
/// # Errors
///
/// - Returns [`ErrorCode::InvalidArgument`] if `path` is relative or if
///   the canonicalized path contains `..` components (symlink escape).
/// - Returns [`ErrorCode::FileNotFound`] if `std::fs::canonicalize`
///   fails — typically because the file does not exist or a parent
///   directory is not traversable.
pub fn validate_local_path(path: &str) -> Result<PathBuf, McpError> {
    let pb = PathBuf::from(path);
    if !pb.is_absolute() {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!(
                "Attach path '{path}' must be absolute. \
                 Pass a full path to a local .hyper file."
            ),
        ));
    }
    let canonical = std::fs::canonicalize(&pb).map_err(|e| {
        McpError::new(
            ErrorCode::FileNotFound,
            format!("Cannot resolve attach path '{path}': {e}"),
        )
    })?;
    if canonical
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!("Attach path '{path}' resolves to a location containing '..' components"),
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_accepts_valid_identifiers() {
        for a in ["src", "_scratch", "data_2024", "A", "alpha_beta_1"] {
            validate_alias(a).unwrap_or_else(|e| panic!("expected {a:?} to be accepted: {e}"));
        }
    }

    #[test]
    fn alias_rejects_reserved_local() {
        assert!(matches!(
            validate_alias("local").unwrap_err().code,
            ErrorCode::InvalidArgument
        ));
        assert!(matches!(
            validate_alias("LOCAL").unwrap_err().code,
            ErrorCode::InvalidArgument
        ));
    }

    #[test]
    fn alias_rejects_bad_shapes() {
        for a in [
            "",
            "1abc",
            "has space",
            "a-b",
            "a.b",
            "a\"b",
            &"a".repeat(64),
        ] {
            let err = validate_alias(a).expect_err(&format!("expected {a:?} to be rejected"));
            assert_eq!(err.code, ErrorCode::InvalidArgument, "alias={a:?}");
        }
    }

    #[test]
    fn path_rejects_relative() {
        let err = validate_local_path("relative/path.hyper").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn path_rejects_missing() {
        // Build an absolute path that is guaranteed not to exist on any OS.
        let missing = std::env::temp_dir().join("hyper_mcp_definitely_missing_99999.hyper");
        let err = validate_local_path(missing.to_str().unwrap()).unwrap_err();
        assert_eq!(err.code, ErrorCode::FileNotFound);
    }

    #[test]
    fn path_canonicalizes_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sample.hyper");
        std::fs::write(&file, b"").unwrap();
        // Construct a path with a `.` component that canonicalization
        // should flatten.
        let noisy = dir.path().join(".").join("sample.hyper");
        let resolved = validate_local_path(noisy.to_str().unwrap()).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn attached_db_to_json_round_trip() {
        let entry = AttachedDb {
            alias: "src".into(),
            source: AttachSource::LocalFile {
                path: PathBuf::from("/tmp/foo.hyper"),
            },
            writable: false,
            attached_at: SystemTime::UNIX_EPOCH,
        };
        let j = entry.to_json();
        assert_eq!(j["alias"], "src");
        assert_eq!(j["writable"], false);
        assert_eq!(j["kind"], "local_file");
        assert_eq!(j["source"]["kind"], "local_file");
        assert_eq!(j["source"]["path"], "/tmp/foo.hyper");
    }

    // -----------------------------------------------------------------
    // validate_input_path / validate_output_path
    // -----------------------------------------------------------------

    #[test]
    fn validate_input_path_rejects_relative() {
        let err = validate_input_path("relative/path.csv", "data file").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(err.message.contains("data file"));
    }

    #[test]
    fn validate_input_path_rejects_missing() {
        // Build a platform-portable absolute path to a nonexistent file.
        // Hardcoded "/definitely/..." paths are not absolute on Windows
        // (no drive letter), so they fail the wrong gate.
        let missing = std::env::temp_dir().join("hyper_mcp_validate_input_missing_99999.csv");
        let err = validate_input_path(missing.to_str().unwrap(), "data file").unwrap_err();
        assert_eq!(err.code, ErrorCode::FileNotFound);
    }

    #[test]
    fn validate_input_path_accepts_existing_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let canonical = validate_input_path(f.path().to_str().unwrap(), "data file").unwrap();
        assert!(canonical.is_absolute());
    }

    #[test]
    fn validate_input_path_kind_appears_in_error() {
        let err = validate_input_path("relative.csv", "iceberg table").unwrap_err();
        assert!(
            err.message.contains("iceberg table"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn validate_output_path_rejects_relative() {
        let err = validate_output_path("relative/out.csv", "export").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn validate_output_path_accepts_nonexistent_with_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("does-not-exist-yet.csv");
        let canonical =
            validate_output_path(target.to_str().unwrap(), "export").expect("should accept");
        assert!(canonical.is_absolute());
        // The returned path should point at our target (canonical parent + name).
        assert_eq!(canonical.file_name(), target.file_name());
    }

    #[test]
    fn validate_output_path_rejects_missing_parent() {
        // Build a platform-portable absolute path with a missing parent.
        // Hardcoded "/definitely/..." paths are not absolute on Windows.
        let missing_parent = std::env::temp_dir()
            .join("hyper_mcp_validate_output_missing_parent_99999")
            .join("out.csv");
        let err = validate_output_path(missing_parent.to_str().unwrap(), "export").unwrap_err();
        assert_eq!(err.code, ErrorCode::FileNotFound);
    }

    #[test]
    fn validate_output_path_accepts_existing_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let canonical = validate_output_path(f.path().to_str().unwrap(), "export").unwrap();
        assert!(canonical.is_absolute());
    }
}
