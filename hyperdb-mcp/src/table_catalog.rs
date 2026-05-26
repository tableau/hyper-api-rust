// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! User-visible catalog of data tables in the workspace.
//!
//! Tracks, for every user-facing table:
//!
//! | Field              | Who populates |
//! |--------------------|---------------|
//! | `table_name`       | MCP (stub)    |
//! | `load_tool`        | MCP (stub)    |
//! | `load_params`      | MCP (stub)    |
//! | `loaded_at`        | MCP (stub)    |
//! | `last_refreshed_at`| MCP (stub, bumped on every explicit load) |
//! | `row_count`        | MCP (stub, refreshed opportunistically) |
//! | `source_url`       | User / LLM via `set_table_metadata` |
//! | `source_description` | User / LLM |
//! | `purpose`          | User / LLM |
//! | `license`          | User / LLM |
//! | `notes`            | User / LLM |
//!
//! The backing table is `_table_catalog`. Each writable database that
//! receives an ingest gets its own catalog, lazily seeded on first
//! ingest (or first `set_table_metadata`):
//!
//! - **Ephemeral primary writes** stub into the persistent catalog
//!   (so a long-running session's bookkeeping survives the
//!   ephemeral file's per-session deletion). Reconciliation cleans
//!   up rows whose tables no longer exist on engine bootstrap.
//! - **`database = "persistent"`** writes to the persistent catalog
//!   directly (same destination as the ephemeral case).
//! - **`database = "<user-attached-writable>"`** writes to that DB's
//!   own `_table_catalog`. Read-only attachments never get a catalog.
//! - **`--ephemeral-only` mode (no persistent attached)** is a
//!   degraded mode where catalog operations are no-ops because there
//!   is nowhere durable to put bookkeeping.
//!
//! Per-DB catalog routing is exposed via the `*_in` variants
//! ([`ensure_exists_in`], [`upsert_stub_in`], [`set_metadata_in`],
//! [`get_in`], [`list_in`], [`delete_for_in`], [`reconcile_in`]). The
//! original names ([`ensure_exists`], [`upsert_stub`], …) become
//! 1-line wrappers passing `target_db = None` (which resolves to the
//! persistent catalog).
//!
//! `_table_catalog` is hidden from the user-visible
//! [`Engine::describe_tables`] output (see
//! [`crate::engine::is_internal_table`]) so the LLM doesn't see it
//! alongside its data tables; users who want to inspect it directly
//! can run `SELECT * FROM "<db>"."public"."_table_catalog"`.
//!
//! # Concurrency
//!
//! Catalog upserts use an optimistic UPDATE-then-conditional-INSERT
//! pattern. Hyper rejects PRIMARY KEY ("Index support is disabled")
//! and `INSERT … ON CONFLICT` ("syntax error: got ON"), but supports
//! `INSERT … SELECT … WHERE NOT EXISTS (…)` as a single atomic
//! statement. Because `hyperd` serializes individual statements,
//! the conditional INSERT can never produce duplicate rows — even
//! when multiple MCP server processes race to upsert the same
//! `table_name` concurrently. The UPDATE uses last-writer-wins
//! semantics for mechanical fields while preserving prose columns
//! untouched.
//!
//! [`HyperMcpServer`]: crate::server::HyperMcpServer

use std::fmt::Write as _;

use crate::engine::{is_internal_table, Engine};
use crate::error::{ErrorCode, McpError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Backing table name. Lives in the persistent attachment under
/// `"persistent"."public"."_table_catalog"`. Hidden from
/// `Engine::describe_tables`; users who want to inspect it directly can
/// run a fully-qualified SELECT.
pub const TABLE_CATALOG_TABLE: &str = "_table_catalog";

/// Returns the fully-qualified SQL reference for `_table_catalog` in
/// `target_db`, or `None` when no durable destination exists.
///
/// Routing:
/// - `None` → persistent catalog if attached, else `None` (the
///   `--ephemeral-only` case, where catalog operations are no-ops).
/// - `Some("persistent")` (case-insensitive) → persistent catalog.
/// - `Some(alias)` → `"<alias>"."public"."_table_catalog"`. The
///   caller is responsible for ensuring the catalog table exists in
///   that DB (typically by calling [`ensure_exists_in`] first); this
///   function does not validate existence or writability.
fn qualified_catalog_in(engine: &Engine, target_db: Option<&str>) -> Option<String> {
    let alias = match target_db {
        None | Some("") => {
            if engine.has_persistent() {
                Engine::PERSISTENT_ALIAS.to_string()
            } else {
                return None;
            }
        }
        Some(a) if a.eq_ignore_ascii_case(Engine::PERSISTENT_ALIAS) => {
            // Persistent is only valid when the engine actually has it.
            if !engine.has_persistent() {
                return None;
            }
            Engine::PERSISTENT_ALIAS.to_string()
        }
        // User-attached alias: lowercase to match the form
        // `AttachRegistry::attach` stores. Hyper is case-sensitive on
        // quoted identifiers, so `"MyDB"` and `"mydb"` are different
        // databases; the registry's lowercase storage is the only
        // form ATTACH actually used.
        Some(a) => a.to_ascii_lowercase(),
    };
    let alias_esc = alias.replace('"', "\"\"");
    Some(format!(
        "\"{alias_esc}\".\"public\".\"{TABLE_CATALOG_TABLE}\""
    ))
}

/// One row in [`TABLE_CATALOG_TABLE`].
///
/// The `Option` fields are `NULL` in the backing table when not set. Prose
/// fields (source description, purpose, etc.) stay `None` on auto-stubbed
/// rows until the user calls `set_table_metadata`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogEntry {
    pub table_name: String,
    pub source_url: Option<String>,
    pub source_description: Option<String>,
    pub purpose: Option<String>,
    pub load_tool: Option<String>,
    pub load_params: Option<String>,
    pub license: Option<String>,
    pub loaded_at: DateTime<Utc>,
    pub last_refreshed_at: DateTime<Utc>,
    pub row_count: Option<i64>,
    pub notes: Option<String>,
    pub created_by: Option<String>,
    pub last_modified_by: Option<String>,
}

impl CatalogEntry {
    /// JSON shape returned by the `set_table_metadata` tool and readable
    /// resources. Times are emitted as RFC 3339 strings so clients don't
    /// need to know Hyper's internal TIMESTAMP format.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "table_name": self.table_name,
            "source_url": self.source_url,
            "source_description": self.source_description,
            "purpose": self.purpose,
            "load_tool": self.load_tool,
            "load_params": self.load_params,
            "license": self.license,
            "loaded_at": self.loaded_at.to_rfc3339(),
            "last_refreshed_at": self.last_refreshed_at.to_rfc3339(),
            "row_count": self.row_count,
            "notes": self.notes,
            "created_by": self.created_by,
            "last_modified_by": self.last_modified_by,
        })
    }
}

/// Partial update payload for [`set_metadata`]. `None` means "leave the
/// existing value alone"; `Some(String::new())` clears the field.
#[derive(Debug, Default, Clone)]
pub struct MetadataFields {
    pub source_url: Option<String>,
    pub source_description: Option<String>,
    pub purpose: Option<String>,
    pub license: Option<String>,
    pub notes: Option<String>,
}

impl MetadataFields {
    /// `true` if every field is `None`, i.e. the caller supplied nothing
    /// to update. Used to short-circuit with a clear error instead of
    /// running a no-op UPDATE.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.source_url.is_none()
            && self.source_description.is_none()
            && self.purpose.is_none()
            && self.license.is_none()
            && self.notes.is_none()
    }
}

// --- Table lifecycle --------------------------------------------------------

/// Column specification shared by [`ensure_exists`] and
/// [`ensure_exists_in_database`]. Kept as a single string constant so
/// the two DDL paths cannot drift out of sync.
const CATALOG_COLUMNS: &str = "(\
     table_name         TEXT NOT NULL, \
     source_url         TEXT, \
     source_description TEXT, \
     purpose            TEXT, \
     load_tool          TEXT, \
     load_params        TEXT, \
     license            TEXT, \
     loaded_at          TIMESTAMP NOT NULL, \
     last_refreshed_at  TIMESTAMP NOT NULL, \
     row_count          BIGINT, \
     notes              TEXT, \
     created_by         TEXT, \
     last_modified_by   TEXT\
 )";

/// Idempotently create the backing table in `target_db`. Safe to call
/// on every engine init or every catalog write — if the table
/// already exists this is a no-op. The schema is the only one the
/// code targets; all prose columns are nullable, so a plain `NULL`
/// insert is always well-formed.
///
/// Routing follows [`qualified_catalog_in`]: `None` resolves to the
/// persistent catalog (or no-op in `--ephemeral-only` mode);
/// `Some("persistent")` is the same; `Some(alias)` targets the
/// attached database directly.
///
/// # Errors
///
/// Propagates any error from [`Engine::execute_command`] on the
/// `CREATE TABLE IF NOT EXISTS` statement — typically connection loss
/// or permission failures (the latter for read-only attachments;
/// callers should pre-check writability).
pub fn ensure_exists_in(engine: &Engine, target_db: Option<&str>) -> Result<(), McpError> {
    let Some(qualified) = qualified_catalog_in(engine, target_db) else {
        // Nowhere durable to put the catalog (--ephemeral-only).
        return Ok(());
    };
    let ddl = format!("CREATE TABLE IF NOT EXISTS {qualified} {CATALOG_COLUMNS}");
    engine.execute_command(&ddl)?;
    // Migrate pre-existing catalogs: add columns introduced after the
    // initial schema. Each ALTER is idempotent — if the column already
    // exists Hyper returns an error that we swallow.
    for col in ["created_by TEXT", "last_modified_by TEXT"] {
        let alter = format!("ALTER TABLE {qualified} ADD COLUMN {col}");
        let _ = engine.execute_command(&alter);
    }
    // After CREATE TABLE IF NOT EXISTS the catalog is guaranteed
    // present in this DB — short-circuit subsequent existence probes.
    if let Some(alias) = resolve_catalog_alias(engine, target_db) {
        engine.mark_catalog_present_for(&alias);
    }
    Ok(())
}

/// Backward-compatible wrapper: ensure the catalog exists in the
/// persistent attachment (or no-op in `--ephemeral-only` mode).
///
/// New code should prefer [`ensure_exists_in`] with an explicit
/// `target_db`.
///
/// # Errors
///
/// Same as [`ensure_exists_in`].
pub fn ensure_exists(engine: &Engine) -> Result<(), McpError> {
    ensure_exists_in(engine, None)
}

/// Backward-compatible wrapper for the old `ensure_exists_in_database`
/// name. Delegates to [`ensure_exists_in`].
///
/// # Errors
///
/// Same as [`ensure_exists_in`].
#[deprecated(since = "0.2.0", note = "use ensure_exists_in(engine, Some(db_alias))")]
pub fn ensure_exists_in_database(engine: &Engine, db_alias: &str) -> Result<(), McpError> {
    ensure_exists_in(engine, Some(db_alias))
}

// --- Reads ------------------------------------------------------------------

/// Fetch every catalog row from `target_db` in name-sorted order.
/// Returns an empty `Vec` when no catalog exists in the target.
///
/// Routing follows [`qualified_catalog_in`].
///
/// # Errors
///
/// - Propagates any error from `table_present_in` (connection failure
///   during the existence probe).
/// - Propagates any error from [`Engine::execute_query_to_json`].
/// - Propagates [`ErrorCode::SchemaMismatch`] from `row_to_entry` if
///   a persisted row cannot be decoded into a [`CatalogEntry`].
pub fn list_in(engine: &Engine, target_db: Option<&str>) -> Result<Vec<CatalogEntry>, McpError> {
    let Some(qualified) = qualified_catalog_in(engine, target_db) else {
        return Ok(Vec::new());
    };
    if !table_present_in(engine, target_db)? {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT table_name, source_url, source_description, purpose, \
                load_tool, load_params, license, loaded_at, last_refreshed_at, \
                row_count, notes, created_by, last_modified_by \
         FROM {qualified} ORDER BY table_name"
    );
    let rows = engine.execute_query_to_json(&sql)?;
    rows.iter().map(row_to_entry).collect()
}

/// Backward-compatible wrapper: list rows from the persistent catalog.
///
/// # Errors
///
/// Same as [`list_in`].
pub fn list(engine: &Engine) -> Result<Vec<CatalogEntry>, McpError> {
    list_in(engine, None)
}

/// Fetch a single row from `target_db` by `table_name`, or `Ok(None)`
/// if absent.
///
/// # Errors
///
/// Same as [`list_in`].
pub fn get_in(
    engine: &Engine,
    table_name: &str,
    target_db: Option<&str>,
) -> Result<Option<CatalogEntry>, McpError> {
    let Some(qualified) = qualified_catalog_in(engine, target_db) else {
        return Ok(None);
    };
    if !table_present_in(engine, target_db)? {
        return Ok(None);
    }
    let sql = format!(
        "SELECT table_name, source_url, source_description, purpose, \
                load_tool, load_params, license, loaded_at, last_refreshed_at, \
                row_count, notes, created_by, last_modified_by \
         FROM {qualified} WHERE table_name = {}",
        sql_literal(table_name)
    );
    let rows = engine.execute_query_to_json(&sql)?;
    match rows.first() {
        Some(row) => Ok(Some(row_to_entry(row)?)),
        None => Ok(None),
    }
}

/// Backward-compatible wrapper: read from the persistent catalog.
///
/// # Errors
///
/// Same as [`get_in`].
pub fn get(engine: &Engine, table_name: &str) -> Result<Option<CatalogEntry>, McpError> {
    get_in(engine, table_name, None)
}

// --- Writes -----------------------------------------------------------------

/// Upsert a catalog row for `table_name` in `target_db`, carrying
/// forward prose fields from any existing row and refreshing
/// mechanical fields.
///
/// Routing follows [`qualified_catalog_in`].
///
/// * `load_tool` / `load_params` — overwrite the existing values.
/// * `row_count` — overwrite.
/// * `loaded_at` — preserve existing value if the row is already present,
///   otherwise set to `now`.
/// * `last_refreshed_at` — set to `now` when `bump_refresh` is `true`,
///   otherwise preserve the existing value (or `now` for new rows).
/// * Prose fields (`source_url`, `source_description`, `purpose`,
///   `license`, `notes`) — preserved unchanged from the existing row.
///
/// Implementation uses optimistic concurrency: UPDATE (last writer
/// wins for existing rows) followed by a conditional INSERT that
/// only fires when no row exists. Each statement is individually
/// atomic at the hyperd level, so concurrent upserts from multiple
/// MCP server processes cannot produce duplicate rows — even without
/// an external lock or transaction wrapper.
///
/// # Errors
///
/// - Propagates errors from [`ensure_exists_in`].
/// - Propagates any error from the UPDATE or INSERT statements —
///   typically connection-loss failures.
pub fn upsert_stub_in(
    engine: &Engine,
    table_name: &str,
    load_tool: &str,
    load_params: Option<&str>,
    row_count: Option<i64>,
    bump_refresh: bool,
    target_db: Option<&str>,
    client_name: Option<&str>,
) -> Result<(), McpError> {
    ensure_exists_in(engine, target_db)?;
    let Some(qualified) = qualified_catalog_in(engine, target_db) else {
        return Ok(());
    };

    let now = Utc::now();
    let row_count_sql = row_count.map_or_else(|| "NULL".into(), |n: i64| n.to_string());
    let client_sql = opt_sql_literal(client_name);

    // Step 1: UPDATE mechanical fields on the existing row. Prose
    // fields are left untouched. `created_by` is never overwritten.
    let set_clauses = if bump_refresh {
        let mut s = format!(
            "load_tool = {}, load_params = {}, row_count = {}, \
             last_refreshed_at = TIMESTAMP {}",
            sql_literal(load_tool),
            opt_sql_literal(load_params),
            row_count_sql,
            sql_literal(&format_timestamp(now)),
        );
        if let Some(name) = client_name {
            let _ = write!(s, ", last_modified_by = {}", sql_literal(name));
        }
        s
    } else {
        let mut s = format!(
            "load_tool = {}, load_params = {}, row_count = {}",
            sql_literal(load_tool),
            opt_sql_literal(load_params),
            row_count_sql,
        );
        if let Some(name) = client_name {
            let _ = write!(s, ", last_modified_by = {}", sql_literal(name));
        }
        s
    };

    let update_sql = format!(
        "UPDATE {qualified} SET {set_clauses} WHERE table_name = {}",
        sql_literal(table_name),
    );
    let updated = engine.execute_command(&update_sql)?;

    // Step 2: If no row existed, conditionally INSERT a new one.
    // The WHERE NOT EXISTS guard prevents duplicates when multiple
    // processes race to insert the same table_name concurrently —
    // hyperd serializes individual statements, so exactly one INSERT
    // will see the absence and succeed.
    if updated == 0 {
        let insert_sql = format!(
            "INSERT INTO {qualified} \
             (table_name, source_url, source_description, purpose, load_tool, \
              load_params, license, loaded_at, last_refreshed_at, row_count, notes, \
              created_by, last_modified_by) \
             SELECT {name}, NULL, NULL, NULL, {load_tool}, \
                    {load_params}, NULL, TIMESTAMP {loaded_at}, TIMESTAMP {last_refreshed_at}, \
                    {row_count}, NULL, {created_by}, {modified_by} \
             WHERE NOT EXISTS (SELECT 1 FROM {qualified} WHERE table_name = {name})",
            name = sql_literal(table_name),
            load_tool = sql_literal(load_tool),
            load_params = opt_sql_literal(load_params),
            loaded_at = sql_literal(&format_timestamp(now)),
            last_refreshed_at = sql_literal(&format_timestamp(now)),
            row_count = row_count_sql,
            created_by = client_sql,
            modified_by = client_sql,
        );
        engine.execute_command(&insert_sql)?;
    }
    Ok(())
}

/// Backward-compatible wrapper: upsert into the persistent catalog.
///
/// # Errors
///
/// Same as [`upsert_stub_in`].
pub fn upsert_stub(
    engine: &Engine,
    table_name: &str,
    load_tool: &str,
    load_params: Option<&str>,
    row_count: Option<i64>,
    bump_refresh: bool,
) -> Result<(), McpError> {
    upsert_stub_in(
        engine,
        table_name,
        load_tool,
        load_params,
        row_count,
        bump_refresh,
        None,
        None,
    )
}

/// Partial UPDATE of prose fields for one table in `target_db`. Errors
/// with [`ErrorCode::TableNotFound`] if there is no catalog row for
/// `table_name` in that DB.
///
/// Routing follows [`qualified_catalog_in`]. The catalog is lazily
/// seeded in `target_db` if absent (matches today's behavior for
/// the persistent target).
///
/// # Errors
///
/// - Returns [`ErrorCode::EmptyData`] if `fields` contains no values
///   to update.
/// - Returns [`ErrorCode::TableNotFound`] if no catalog row exists for
///   `table_name` in `target_db`.
/// - Returns [`ErrorCode::ReadOnlyViolation`] when there is no durable
///   catalog destination (`--ephemeral-only` with `target_db = None`).
/// - Propagates any error from [`ensure_exists_in`], [`get_in`], or the
///   `UPDATE` statement.
pub fn set_metadata_in(
    engine: &Engine,
    table_name: &str,
    fields: &MetadataFields,
    target_db: Option<&str>,
) -> Result<CatalogEntry, McpError> {
    // Validate caller intent BEFORE seeding the catalog. Both the
    // empty-fields and missing-row paths return an error; we don't
    // want them to mutate the target DB's schema as a side effect.
    if fields.is_empty() {
        return Err(McpError::new(
            ErrorCode::EmptyData,
            "set_table_metadata requires at least one of source_url, \
             source_description, purpose, license, notes",
        ));
    }

    // Require an existing row so we don't accidentally create catalog
    // entries for tables that don't exist. The server wires the catalog
    // up to ingest + execute so any real table should already have a
    // stub row. `get_in` returns None when the catalog table itself
    // doesn't exist yet; treat that as "no row" without seeding the
    // catalog (the user gets a clean TableNotFound and the .hyper file
    // is left untouched).
    let existing = get_in(engine, table_name, target_db)?.ok_or_else(|| {
        let where_clause = match target_db {
            Some(alias) if !alias.eq_ignore_ascii_case(Engine::PERSISTENT_ALIAS) => {
                format!(" in database '{alias}'")
            }
            _ => String::new(),
        };
        McpError::new(
            ErrorCode::TableNotFound,
            format!(
                "No catalog entry for table '{table_name}'{where_clause}. Load the \
                 table first (load_file / load_data / execute CREATE TABLE) or \
                 create it and re-run; the catalog is refreshed automatically on \
                 those paths."
            ),
        )
    })?;

    let mut assignments: Vec<String> = Vec::new();
    if let Some(v) = &fields.source_url {
        assignments.push(format!("source_url = {}", sql_literal_or_null_if_empty(v)));
    }
    if let Some(v) = &fields.source_description {
        assignments.push(format!(
            "source_description = {}",
            sql_literal_or_null_if_empty(v)
        ));
    }
    if let Some(v) = &fields.purpose {
        assignments.push(format!("purpose = {}", sql_literal_or_null_if_empty(v)));
    }
    if let Some(v) = &fields.license {
        assignments.push(format!("license = {}", sql_literal_or_null_if_empty(v)));
    }
    if let Some(v) = &fields.notes {
        assignments.push(format!("notes = {}", sql_literal_or_null_if_empty(v)));
    }

    let qualified = qualified_catalog_in(engine, target_db).ok_or_else(|| {
        McpError::new(
            ErrorCode::ReadOnlyViolation,
            "set_table_metadata is unavailable in --ephemeral-only mode \
             because the catalog has nowhere durable to live.",
        )
    })?;
    let update_sql = format!(
        "UPDATE {qualified} SET {assigns} WHERE table_name = {name}",
        assigns = assignments.join(", "),
        name = sql_literal(table_name),
    );
    engine.execute_command(&update_sql)?;

    // Re-read so the caller gets the canonical view (including unchanged
    // fields). `existing` is our fallback if the re-read somehow returns
    // nothing — that shouldn't happen, but preserves the previous row
    // instead of failing spuriously.
    get_in(engine, table_name, target_db)?.map_or(Ok(existing), Ok)
}

/// Backward-compatible wrapper: update prose fields in the persistent
/// catalog.
///
/// # Errors
///
/// Same as [`set_metadata_in`].
pub fn set_metadata(
    engine: &Engine,
    table_name: &str,
    fields: &MetadataFields,
) -> Result<CatalogEntry, McpError> {
    set_metadata_in(engine, table_name, fields, None)
}

/// Delete the catalog row (if any) for `table_name` in `target_db`.
/// Idempotent — no error if the row doesn't exist.
///
/// # Errors
///
/// Propagates any error from `table_present_in` or from the `DELETE`.
pub fn delete_for_in(
    engine: &Engine,
    table_name: &str,
    target_db: Option<&str>,
) -> Result<bool, McpError> {
    let Some(qualified) = qualified_catalog_in(engine, target_db) else {
        return Ok(false);
    };
    if !table_present_in(engine, target_db)? {
        return Ok(false);
    }
    let sql = format!(
        "DELETE FROM {qualified} WHERE table_name = {}",
        sql_literal(table_name)
    );
    let affected = engine.execute_command(&sql)?;
    Ok(affected > 0)
}

/// Backward-compatible wrapper: delete from the persistent catalog.
///
/// # Errors
///
/// Same as [`delete_for_in`].
pub fn delete_for(engine: &Engine, table_name: &str) -> Result<bool, McpError> {
    delete_for_in(engine, table_name, None)
}

/// Synchronize the catalog in `target_db` against the current set of
/// user tables in that DB.
///
/// * Insert a stub row for every user table missing from the catalog.
///   These stubs use `load_tool = "unknown"` so callers can later tell
///   the difference between "loaded via a tool and we tracked it" and
///   "found during reconciliation".
/// * Delete catalog rows whose table no longer exists in `target_db`.
/// * Refresh `row_count` on every remaining row.
///
/// Does *not* bump `last_refreshed_at` — reconciliation is a housekeeping
/// pass, not a data refresh.
///
/// # Errors
///
/// - Propagates any error from [`ensure_exists_in`], `user_tables_in`,
///   [`list_in`], [`delete_for_in`], `refresh_row_count_in`, or
///   [`upsert_stub_in`].
/// - `row_count_of_in` failures are swallowed (the row count falls
///   back to `None`).
pub fn reconcile_in(engine: &Engine, target_db: Option<&str>) -> Result<(), McpError> {
    ensure_exists_in(engine, target_db)?;

    let tables = user_tables_in(engine, target_db)?;
    let catalog_entries = list_in(engine, target_db)?;
    let catalog_names: std::collections::HashSet<String> = catalog_entries
        .iter()
        .map(|e| e.table_name.clone())
        .collect();
    let live_tables: std::collections::HashSet<String> = tables.iter().cloned().collect();

    for entry in &catalog_entries {
        if !live_tables.contains(&entry.table_name) {
            delete_for_in(engine, &entry.table_name, target_db)?;
        }
    }

    for table in &tables {
        let row_count = row_count_of_in(engine, table, target_db).ok();
        if catalog_names.contains(table) {
            refresh_row_count_in(engine, table, row_count, target_db)?;
        } else {
            upsert_stub_in(
                engine, table, "unknown", None, row_count, false, target_db, None,
            )?;
        }
    }
    Ok(())
}

/// Backward-compatible wrapper: reconcile the persistent catalog.
///
/// # Errors
///
/// Same as [`reconcile_in`].
pub fn reconcile(engine: &Engine) -> Result<(), McpError> {
    reconcile_in(engine, None)
}

// --- Internals --------------------------------------------------------------

/// List user-facing tables in the **persistent** attachment (excludes
/// `_hyperdb_*` internals and the catalog itself). Returns an empty Vec
/// when no persistent attachment is present. Uses a fully-qualified
/// `pg_catalog.pg_tables` probe so it sees inside the attachment;
/// `Catalog::get_table_names` would target the connection's primary
/// (ephemeral) by default.
/// List user-facing tables in `target_db` (excludes `_hyperdb_*`
/// internals and the catalog itself). Returns an empty Vec when no
/// catalog destination exists for the target.
fn user_tables_in(engine: &Engine, target_db: Option<&str>) -> Result<Vec<String>, McpError> {
    let Some(alias) = resolve_catalog_alias(engine, target_db) else {
        return Ok(Vec::new());
    };
    let alias_esc = alias.replace('"', "\"\"");
    let sql = format!(
        "SELECT tablename FROM \"{alias_esc}\".pg_catalog.pg_tables \
         WHERE schemaname = 'public'"
    );
    let rows = engine.execute_query_to_json(&sql)?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            r.get("tablename")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .filter(|name| name != TABLE_CATALOG_TABLE && !is_internal_table(name))
        .collect())
}

/// `true` when `_table_catalog` is present inside `target_db`.
/// Returns `false` when no catalog destination exists.
///
/// Caches the per-DB result on the engine after the first probe so
/// subsequent reads/writes skip the existence query. Catalog
/// mutators ([`ensure_exists_in`]) update the cache directly via
/// [`Engine::mark_catalog_present_for`].
fn table_present_in(engine: &Engine, target_db: Option<&str>) -> Result<bool, McpError> {
    let Some(alias) = resolve_catalog_alias(engine, target_db) else {
        return Ok(false);
    };
    let alias_for_probe = alias.clone();
    engine.catalog_present_in(&alias, move |engine| {
        let alias_esc = alias_for_probe.replace('"', "\"\"");
        let sql = format!(
            "SELECT tablename FROM \"{alias_esc}\".pg_catalog.pg_tables \
             WHERE schemaname = 'public' AND tablename = {}",
            sql_literal(TABLE_CATALOG_TABLE)
        );
        let rows = engine.execute_query_to_json(&sql)?;
        Ok(!rows.is_empty())
    })
}

/// Return `COUNT(*)` for a user table inside `target_db`. Quoted to
/// handle mixed-case or keyword-like names. Returns 0 if no catalog
/// destination exists.
fn row_count_of_in(
    engine: &Engine,
    table_name: &str,
    target_db: Option<&str>,
) -> Result<i64, McpError> {
    let Some(alias) = resolve_catalog_alias(engine, target_db) else {
        return Ok(0);
    };
    let alias_esc = alias.replace('"', "\"\"");
    let quoted = table_name.replace('"', "\"\"");
    let sql = format!("SELECT COUNT(*) AS cnt FROM \"{alias_esc}\".\"public\".\"{quoted}\"");
    let rows = engine.execute_query_to_json(&sql)?;
    Ok(rows
        .first()
        .and_then(|r| r.get("cnt").and_then(serde_json::Value::as_i64))
        .unwrap_or(0))
}

/// Cheap UPDATE for just the `row_count` column of an existing row in
/// `target_db`. Used by [`reconcile_in`].
fn refresh_row_count_in(
    engine: &Engine,
    table_name: &str,
    row_count: Option<i64>,
    target_db: Option<&str>,
) -> Result<(), McpError> {
    let Some(qualified) = qualified_catalog_in(engine, target_db) else {
        return Ok(());
    };
    let sql = format!(
        "UPDATE {qualified} SET row_count = {count} WHERE table_name = {name}",
        count = row_count.map_or_else(|| "NULL".into(), |n| n.to_string()),
        name = sql_literal(table_name),
    );
    engine.execute_command(&sql)?;
    Ok(())
}

/// Resolve `target_db` to the canonical alias whose catalog this
/// operation should target. Mirrors [`qualified_catalog_in`]'s
/// routing but returns just the alias (not the qualified table
/// reference) for callers that need to build their own SQL.
fn resolve_catalog_alias(engine: &Engine, target_db: Option<&str>) -> Option<String> {
    match target_db {
        None | Some("") => {
            if engine.has_persistent() {
                Some(Engine::PERSISTENT_ALIAS.to_string())
            } else {
                None
            }
        }
        Some(a) if a.eq_ignore_ascii_case(Engine::PERSISTENT_ALIAS) => {
            if engine.has_persistent() {
                Some(Engine::PERSISTENT_ALIAS.to_string())
            } else {
                None
            }
        }
        // User-attached alias: lowercase to match the registry's
        // canonical storage form (see `AttachRegistry::attach`).
        Some(a) => Some(a.to_ascii_lowercase()),
    }
}

/// Hyper emits TIMESTAMP columns as either `YYYY-MM-DDTHH:MM:SS...` (RFC
/// 3339) or `YYYY-MM-DD HH:MM:SS[.fff]` depending on path; accept both.
fn parse_timestamp(s: &str) -> Result<DateTime<Utc>, McpError> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
                .map(|ndt| ndt.and_utc())
                .map_err(|e| {
                    McpError::new(
                        ErrorCode::InternalError,
                        format!("Could not parse timestamp '{s}': {e}"),
                    )
                })
        })
}

/// Format a UTC timestamp for Hyper's `TIMESTAMP 'literal'` syntax.
fn format_timestamp(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

fn row_to_entry(row: &Value) -> Result<CatalogEntry, McpError> {
    let table_name = row
        .get("table_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            McpError::new(
                ErrorCode::InternalError,
                "_table_catalog row missing 'table_name'",
            )
        })?
        .to_string();
    let str_field = |name: &str| row.get(name).and_then(|v| v.as_str()).map(str::to_string);
    let loaded_at = parse_timestamp(row.get("loaded_at").and_then(|v| v.as_str()).unwrap_or(""))?;
    let last_refreshed_at = parse_timestamp(
        row.get("last_refreshed_at")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    )?;
    let row_count = row.get("row_count").and_then(serde_json::Value::as_i64);
    Ok(CatalogEntry {
        table_name,
        source_url: str_field("source_url"),
        source_description: str_field("source_description"),
        purpose: str_field("purpose"),
        load_tool: str_field("load_tool"),
        load_params: str_field("load_params"),
        license: str_field("license"),
        loaded_at,
        last_refreshed_at,
        row_count,
        notes: str_field("notes"),
        created_by: str_field("created_by"),
        last_modified_by: str_field("last_modified_by"),
    })
}

/// Escape a SQL string literal for direct concatenation. Matches the
/// approach used by `saved_queries::sql_literal` — `'` is doubled per
/// ANSI SQL, nothing else is special.
fn sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Same as [`sql_literal`] but treats an empty string as an explicit
/// clear — rendered as `NULL` rather than `''`. Used on
/// `set_table_metadata` so callers can wipe a field by passing `""`.
fn sql_literal_or_null_if_empty(s: &str) -> String {
    if s.is_empty() {
        "NULL".into()
    } else {
        sql_literal(s)
    }
}

/// Render an optional prose value into SQL: `NULL` if `None`, otherwise
/// the properly-quoted literal. All prose columns in `_table_catalog`
/// are nullable, so this is always a well-formed INSERT fragment.
fn opt_sql_literal(v: Option<&str>) -> String {
    match v {
        Some(s) => sql_literal(s),
        None => "NULL".into(),
    }
}
