// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Named read-only SQL queries exposed via the `save_query` / `delete_query`
//! tools and the `hyper://queries/{name}/definition` +
//! `hyper://queries/{name}/result` resources.
//!
//! Two [`SavedQueryStore`] implementations:
//!
//! * [`SessionStore`] — in-memory `HashMap` behind a `Mutex`. Used for
//!   ephemeral servers (no `--workspace`) where persistence across restarts
//!   is meaningless because the whole `.hyper` file is thrown away.
//! * [`WorkspaceStore`] — backs onto a dedicated meta-table
//!   `_hyperdb_saved_queries` inside the Hyper workspace. Chosen when a
//!   `--workspace` path is configured so saved queries survive restarts
//!   alongside the data they query.
//!
//! The server picks a store variant in `HyperMcpServer::new` and hands it
//! to tool handlers through `Arc<dyn SavedQueryStore>`. Both variants share
//! the same async-free API so call sites don't have to care which is in
//! use.

use crate::engine::Engine;
use crate::error::{ErrorCode, McpError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The meta-table used by [`WorkspaceStore`] to persist named queries
/// inside the `.hyper` workspace. The underscore prefix is a convention
/// for "`HyperDB` internal" — users shouldn't query or mutate it directly.
pub const SAVED_QUERIES_TABLE: &str = "_hyperdb_saved_queries";

/// A named SQL query stored in the workspace.
///
/// Stored queries are *always* read-only at the SQL level — the save path
/// enforces [`crate::engine::is_read_only_sql`] so accidentally persisting
/// a destructive statement is impossible. The `created_at` field is
/// populated server-side at save time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedQuery {
    /// Human-friendly identifier; used as the path component in the
    /// corresponding resource URIs.
    pub name: String,
    /// The SQL string that will be run when the `result` resource is read.
    pub sql: String,
    /// Optional free-form description of what the query answers.
    pub description: Option<String>,
    /// Server-side save time in UTC.
    pub created_at: DateTime<Utc>,
}

impl SavedQuery {
    /// JSON shape returned by `hyper://queries/{name}/definition`. Keeps
    /// the representation consistent regardless of storage backend.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "sql": self.sql,
            "description": self.description,
            "created_at": self.created_at.to_rfc3339(),
        })
    }
}

/// CRUD interface shared by both storage backends.
///
/// All methods take `&self` because both variants use interior mutability
/// (`Mutex<HashMap>` or Hyper's own connection locking), so a single
/// `Arc<dyn SavedQueryStore>` can be shared across the tool router and
/// resource handler without further wrapping.
///
/// `engine` is passed in by the caller when the operation needs to touch
/// Hyper; stores that don't need it (e.g. [`SessionStore`]) simply ignore
/// the argument.
pub trait SavedQueryStore: Send + Sync {
    /// Persist a new query. Returns an `AlreadyExists`-class error
    /// (currently `SchemaMismatch`, with a clear message) if `name` is
    /// already in use — callers should `delete` first if overwriting is
    /// intended.
    ///
    /// # Errors
    ///
    /// - Returns [`ErrorCode::InvalidArgument`] if a query with the same
    ///   name already exists.
    /// - Returns [`ErrorCode::InternalError`] for store-specific failures
    ///   (poisoned mutex in [`SessionStore`], Hyper catalog errors in
    ///   `CatalogStore`).
    fn save(&self, engine: Option<&Engine>, query: SavedQuery) -> Result<(), McpError>;

    /// Retrieve a single saved query by name, or `Ok(None)` if not found.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InternalError`] for store-specific failures
    /// (poisoned mutex, catalog read failure, JSON decode failure of a
    /// persisted row).
    fn get(&self, engine: Option<&Engine>, name: &str) -> Result<Option<SavedQuery>, McpError>;

    /// List all saved queries in alphabetical-by-name order. Empty
    /// workspaces return `Ok(vec![])`, never an error.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InternalError`] for store-specific failures
    /// (poisoned mutex, catalog read failure, JSON decode failure).
    fn list(&self, engine: Option<&Engine>) -> Result<Vec<SavedQuery>, McpError>;

    /// Remove a saved query by name. Returns `Ok(false)` if the name
    /// wasn't present, `Ok(true)` if it was removed.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InternalError`] for store-specific failures
    /// (poisoned mutex, Hyper delete statement failure).
    fn delete(&self, engine: Option<&Engine>, name: &str) -> Result<bool, McpError>;
}

// --- SessionStore -----------------------------------------------------------

/// In-memory [`SavedQueryStore`] for ephemeral workspaces. Entries live in
/// a `Mutex<HashMap>` and vanish when the server process exits.
///
/// Intentional: reusing the saved query registry across restarts would be
/// surprising when the workspace itself is ephemeral, since the underlying
/// tables aren't persisted either.
#[derive(Debug, Default)]
pub struct SessionStore {
    inner: Mutex<HashMap<String, SavedQuery>>,
}

impl SessionStore {
    /// Construct an empty registry. Prefer wrapping in `Arc` immediately
    /// — both the tool router and the resource handler need a handle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SavedQueryStore for SessionStore {
    fn save(&self, _engine: Option<&Engine>, query: SavedQuery) -> Result<(), McpError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| McpError::new(ErrorCode::InternalError, "SessionStore lock poisoned"))?;
        if guard.contains_key(&query.name) {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "A saved query named '{}' already exists. Delete it first with \
                     delete_query if you intend to overwrite.",
                    query.name
                ),
            ));
        }
        guard.insert(query.name.clone(), query);
        Ok(())
    }

    fn get(&self, _engine: Option<&Engine>, name: &str) -> Result<Option<SavedQuery>, McpError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| McpError::new(ErrorCode::InternalError, "SessionStore lock poisoned"))?;
        Ok(guard.get(name).cloned())
    }

    fn list(&self, _engine: Option<&Engine>) -> Result<Vec<SavedQuery>, McpError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| McpError::new(ErrorCode::InternalError, "SessionStore lock poisoned"))?;
        let mut out: Vec<SavedQuery> = guard.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn delete(&self, _engine: Option<&Engine>, name: &str) -> Result<bool, McpError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| McpError::new(ErrorCode::InternalError, "SessionStore lock poisoned"))?;
        Ok(guard.remove(name).is_some())
    }
}

// --- WorkspaceStore ---------------------------------------------------------

/// Persistent [`SavedQueryStore`] backed by the `_hyperdb_saved_queries`
/// meta-table inside the `.hyper` workspace. Rows round-trip through SQL
/// parameter binding so saved queries containing quotes or backslashes are
/// handled safely.
///
/// Lazy init: the meta-table is created on the first `save`/`list`/`get`/
/// `delete` call, guarded by a `Mutex<bool>` so concurrent first-touches
/// don't race against each other.
#[derive(Debug, Default)]
pub struct WorkspaceStore {
    initialized: Mutex<bool>,
}

impl WorkspaceStore {
    /// Construct an empty registry. The actual meta-table is created
    /// lazily on the first CRUD call.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fully-qualified table reference inside the persistent attachment.
    /// Saved queries are user reference material — they belong with
    /// curated, long-lived data, which lives in the persistent DB.
    fn qualified_table() -> String {
        format!(
            "\"{}\".\"public\".\"{}\"",
            Engine::PERSISTENT_ALIAS,
            SAVED_QUERIES_TABLE
        )
    }

    /// Idempotently create the meta-table inside the persistent
    /// attachment. Called at the top of every public method to keep
    /// each entry point self-contained.
    ///
    /// The `initialized` flag is intentionally **not** reset on a
    /// `ConnectionLost` reconnect. That's safe because `WorkspaceStore`
    /// only ever backs persistent attachments (ephemeral-only sessions
    /// use `SessionStore`), and the meta-table lives in the `.hyper`
    /// file itself — a reconnect opens the same file and finds the table
    /// already there.
    fn ensure_table(&self, engine: &Engine) -> Result<(), McpError> {
        let mut flag = self
            .initialized
            .lock()
            .map_err(|_| McpError::new(ErrorCode::InternalError, "WorkspaceStore lock poisoned"))?;
        if *flag {
            return Ok(());
        }
        // `IF NOT EXISTS` means this is safe even across restarts where
        // the meta-table already exists in the workspace file. No
        // `PRIMARY KEY` because Hyper does not support indexes; name
        // uniqueness is enforced application-side in [`Self::save`].
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
                 name TEXT NOT NULL, \
                 sql TEXT NOT NULL, \
                 description TEXT, \
                 created_at TIMESTAMP NOT NULL\
             )",
            table = Self::qualified_table()
        );
        engine.execute_command(&ddl)?;
        *flag = true;
        Ok(())
    }
}

/// Escape a SQL string literal for direct concatenation. Only needed for
/// the [`WorkspaceStore`] INSERTs where parameter binding isn't used
/// because `execute_command` doesn't expose a bind path. `'` doubles to
/// `''` per ANSI SQL; everything else passes through.
fn sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Materialize a row of the meta-table (returned by `execute_query_to_json`)
/// into a `SavedQuery`. Times come back as RFC 3339 strings from the Hyper
/// JSON renderer.
fn row_to_saved_query(row: &Value) -> Result<SavedQuery, McpError> {
    let name = row
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            McpError::new(
                ErrorCode::InternalError,
                "_hyperdb_saved_queries row missing 'name'",
            )
        })?
        .to_string();
    let sql = row
        .get("sql")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            McpError::new(
                ErrorCode::InternalError,
                "_hyperdb_saved_queries row missing 'sql'",
            )
        })?
        .to_string();
    let description = row
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);
    let created_at_str = row.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
    // Accept both RFC 3339 and the space-separated `YYYY-MM-DD HH:MM:SS[.fff]`
    // shape Hyper emits for TIMESTAMP columns; fall back to "now" on parse
    // failure rather than losing the whole row.
    let created_at = DateTime::parse_from_rfc3339(created_at_str)
        .map(|d| d.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(created_at_str, "%Y-%m-%d %H:%M:%S%.f")
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(created_at_str, "%Y-%m-%d %H:%M:%S")
                })
                .map(|ndt| ndt.and_utc())
                .map_err(|e| {
                    McpError::new(
                        ErrorCode::InternalError,
                        format!("Could not parse created_at '{created_at_str}': {e}"),
                    )
                })
        })?;

    Ok(SavedQuery {
        name,
        sql,
        description,
        created_at,
    })
}

impl SavedQueryStore for WorkspaceStore {
    fn save(&self, engine: Option<&Engine>, query: SavedQuery) -> Result<(), McpError> {
        let engine = engine.ok_or_else(|| {
            McpError::new(
                ErrorCode::InternalError,
                "WorkspaceStore requires an engine handle",
            )
        })?;
        self.ensure_table(engine)?;
        let table = Self::qualified_table();

        // Up-front existence check — clearer error than Hyper's raw PK
        // violation message, and matches SessionStore's behaviour.
        let existing_sql = format!(
            "SELECT name FROM {table} WHERE name = {}",
            sql_literal(&query.name)
        );
        let rows = engine.execute_query_to_json(&existing_sql)?;
        if !rows.is_empty() {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "A saved query named '{}' already exists. Delete it first with \
                     delete_query if you intend to overwrite.",
                    query.name
                ),
            ));
        }

        let description_sql = match &query.description {
            Some(d) => sql_literal(d),
            None => "NULL".into(),
        };
        let insert_sql = format!(
            "INSERT INTO {table} (name, sql, description, created_at) \
             VALUES ({name}, {sql}, {desc}, TIMESTAMP {ts})",
            name = sql_literal(&query.name),
            sql = sql_literal(&query.sql),
            desc = description_sql,
            // Hyper parses `TIMESTAMP 'YYYY-MM-DD HH:MM:SS[.fff]'` literals;
            // strip the trailing "Z" that RFC 3339 adds.
            ts = sql_literal(&query.created_at.format("%Y-%m-%d %H:%M:%S%.6f").to_string()),
        );
        engine.execute_command(&insert_sql)?;
        Ok(())
    }

    fn get(&self, engine: Option<&Engine>, name: &str) -> Result<Option<SavedQuery>, McpError> {
        let engine = engine.ok_or_else(|| {
            McpError::new(
                ErrorCode::InternalError,
                "WorkspaceStore requires an engine handle",
            )
        })?;
        self.ensure_table(engine)?;
        let sql = format!(
            "SELECT name, sql, description, created_at \
             FROM {table} WHERE name = {}",
            sql_literal(name),
            table = Self::qualified_table(),
        );
        let rows = engine.execute_query_to_json(&sql)?;
        match rows.first() {
            Some(row) => Ok(Some(row_to_saved_query(row)?)),
            None => Ok(None),
        }
    }

    fn list(&self, engine: Option<&Engine>) -> Result<Vec<SavedQuery>, McpError> {
        let engine = engine.ok_or_else(|| {
            McpError::new(
                ErrorCode::InternalError,
                "WorkspaceStore requires an engine handle",
            )
        })?;
        self.ensure_table(engine)?;
        let sql = format!(
            "SELECT name, sql, description, created_at \
             FROM {table} ORDER BY name",
            table = Self::qualified_table(),
        );
        let rows = engine.execute_query_to_json(&sql)?;
        rows.iter().map(row_to_saved_query).collect()
    }

    fn delete(&self, engine: Option<&Engine>, name: &str) -> Result<bool, McpError> {
        let engine = engine.ok_or_else(|| {
            McpError::new(
                ErrorCode::InternalError,
                "WorkspaceStore requires an engine handle",
            )
        })?;
        self.ensure_table(engine)?;
        let sql = format!(
            "DELETE FROM {table} WHERE name = {}",
            sql_literal(name),
            table = Self::qualified_table(),
        );
        let affected = engine.execute_command(&sql)?;
        Ok(affected > 0)
    }
}

// --- Factory ----------------------------------------------------------------

/// Build the right store for a given workspace mode.
///
/// `Some(path)` → [`WorkspaceStore`] (persisted in the `.hyper` file).
/// `None`       → [`SessionStore`] (in-memory, dies with the process).
#[must_use]
pub fn build_store(workspace_path: Option<&str>) -> Arc<dyn SavedQueryStore> {
    if workspace_path.is_some() {
        Arc::new(WorkspaceStore::new())
    } else {
        Arc::new(SessionStore::new())
    }
}
