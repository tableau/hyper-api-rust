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
//! The backing table is `_table_catalog` (single underscore, *not*
//! `_hyperdb_`) so it shows up in `describe` and the resource catalog — the
//! catalog is meant to be read by humans and LLMs, it isn't internal
//! bookkeeping. The `_` prefix signals "workspace meta" without triggering
//! [`crate::engine::is_internal_table`]'s hidden-table filter.
//!
//! All operations no-op quietly when [`HyperMcpServer`] was constructed with
//! `--bare`: the module never gets called in that mode, so the table is
//! never created and the workspace file stays pristine.
//!
//! [`HyperMcpServer`]: crate::server::HyperMcpServer

use crate::engine::{is_internal_table, Engine};
use crate::error::{ErrorCode, McpError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Backing table name. Visible in `describe`; users can `SELECT * FROM
/// _table_catalog` directly.
pub const TABLE_CATALOG_TABLE: &str = "_table_catalog";

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
     notes              TEXT\
 )";

/// Idempotently create the backing table in the primary workspace. Safe
/// to call on every engine init — if the table already exists this is
/// a no-op. The schema here is the only one the code targets; all
/// prose columns are nullable, so a plain `NULL` insert is always
/// well-formed.
///
/// The DDL uses the unqualified name, which — thanks to the
/// `schema_search_path` pin installed by [`crate::attach::AttachRegistry`]
/// — always resolves to the primary workspace even while additional
/// databases are attached.
///
/// # Errors
///
/// Propagates any error from [`Engine::execute_command`] on the
/// `CREATE TABLE IF NOT EXISTS` statement — typically connection loss
/// or permission failures.
pub fn ensure_exists(engine: &Engine) -> Result<(), McpError> {
    let ddl = format!("CREATE TABLE IF NOT EXISTS \"{TABLE_CATALOG_TABLE}\" {CATALOG_COLUMNS}");
    engine.execute_command(&ddl)?;
    Ok(())
}

/// Idempotently create `_table_catalog` inside an *attached* database
/// (fully qualified as `"{db_alias}"."public"."_table_catalog"`), with
/// the same schema as the primary's catalog.
///
/// Used by `attach_database` when the MCP just created a fresh
/// `.hyper` file via `on_missing: create`: seeding the empty file with
/// the catalog table makes the new workspace immediately usable as a
/// primary the next time someone opens it on its own, without paying
/// a backfill sweep. Only called when the server is not in bare mode;
/// attaching an *existing* database never touches its `_table_catalog`.
///
/// # Errors
///
/// Propagates any error from the qualified `CREATE TABLE IF NOT EXISTS`
/// against the attached database — typically a wire error or a
/// malformed alias that slipped past validation.
pub fn ensure_exists_in_database(engine: &Engine, db_alias: &str) -> Result<(), McpError> {
    let alias_esc = db_alias.replace('"', "\"\"");
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS \"{alias_esc}\".\"public\".\"{TABLE_CATALOG_TABLE}\" \
         {CATALOG_COLUMNS}"
    );
    engine.execute_command(&ddl)?;
    Ok(())
}

// --- Reads ------------------------------------------------------------------

/// Fetch every catalog row in name-sorted order. Returns an empty `Vec` if
/// the catalog table doesn't exist (callers shouldn't need to pre-check).
///
/// # Errors
///
/// - Propagates any error from `table_present` (connection failure
///   during the existence probe).
/// - Propagates any error from [`Engine::execute_query_to_json`].
/// - Propagates [`ErrorCode::SchemaMismatch`] from `row_to_entry` if
///   a persisted row cannot be decoded into a [`CatalogEntry`].
pub fn list(engine: &Engine) -> Result<Vec<CatalogEntry>, McpError> {
    if !table_present(engine)? {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT table_name, source_url, source_description, purpose, \
                load_tool, load_params, license, loaded_at, last_refreshed_at, \
                row_count, notes \
         FROM \"{TABLE_CATALOG_TABLE}\" ORDER BY table_name"
    );
    let rows = engine.execute_query_to_json(&sql)?;
    rows.iter().map(row_to_entry).collect()
}

/// Fetch a single row by `table_name`, or `Ok(None)` if absent.
///
/// # Errors
///
/// Same as [`list`]: propagates errors from `table_present`, the
/// Hyper `SELECT`, or row decoding.
pub fn get(engine: &Engine, table_name: &str) -> Result<Option<CatalogEntry>, McpError> {
    if !table_present(engine)? {
        return Ok(None);
    }
    let sql = format!(
        "SELECT table_name, source_url, source_description, purpose, \
                load_tool, load_params, license, loaded_at, last_refreshed_at, \
                row_count, notes \
         FROM \"{TABLE_CATALOG_TABLE}\" WHERE table_name = {}",
        sql_literal(table_name)
    );
    let rows = engine.execute_query_to_json(&sql)?;
    match rows.first() {
        Some(row) => Ok(Some(row_to_entry(row)?)),
        None => Ok(None),
    }
}

// --- Writes -----------------------------------------------------------------

/// Upsert a catalog row for `table_name`, carrying forward prose fields
/// from any existing row and refreshing mechanical fields.
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
/// Implementation is DELETE + INSERT (Hyper lacks `UPSERT`), run inside a
/// transaction for atomicity.
///
/// # Errors
///
/// - Propagates errors from [`ensure_exists`] and [`get`] (catalog
///   probe and read failures).
/// - Propagates any transaction error from the enclosing
///   [`Engine::execute_in_transaction`] — typically DELETE, INSERT,
///   commit, or connection-loss failures.
pub fn upsert_stub(
    engine: &Engine,
    table_name: &str,
    load_tool: &str,
    load_params: Option<&str>,
    row_count: Option<i64>,
    bump_refresh: bool,
) -> Result<(), McpError> {
    ensure_exists(engine)?;

    let existing = get(engine, table_name)?;
    let now = Utc::now();
    let loaded_at = existing.as_ref().map_or(now, |e| e.loaded_at);
    let last_refreshed_at = if bump_refresh {
        now
    } else {
        existing.as_ref().map_or(now, |e| e.last_refreshed_at)
    };

    let (source_url, source_description, purpose, license, notes) = match existing.as_ref() {
        Some(e) => (
            e.source_url.clone(),
            e.source_description.clone(),
            e.purpose.clone(),
            e.license.clone(),
            e.notes.clone(),
        ),
        None => (None, None, None, None, None),
    };

    engine.execute_in_transaction(|engine| {
        let delete_sql = format!(
            "DELETE FROM \"{TABLE_CATALOG_TABLE}\" WHERE table_name = {}",
            sql_literal(table_name)
        );
        engine.execute_command(&delete_sql)?;

        let insert_sql = format!(
            "INSERT INTO \"{TABLE_CATALOG_TABLE}\" \
             (table_name, source_url, source_description, purpose, load_tool, \
              load_params, license, loaded_at, last_refreshed_at, row_count, notes) \
             VALUES ({name}, {source_url}, {source_description}, {purpose}, {load_tool}, \
                     {load_params}, {license}, TIMESTAMP {loaded_at}, TIMESTAMP {last_refreshed_at}, \
                     {row_count}, {notes})",
            name = sql_literal(table_name),
            source_url = opt_sql_literal(source_url.as_deref()),
            source_description = opt_sql_literal(source_description.as_deref()),
            purpose = opt_sql_literal(purpose.as_deref()),
            load_tool = sql_literal(load_tool),
            load_params = opt_sql_literal(load_params),
            license = opt_sql_literal(license.as_deref()),
            loaded_at = sql_literal(&format_timestamp(loaded_at)),
            last_refreshed_at = sql_literal(&format_timestamp(last_refreshed_at)),
            row_count = row_count.map_or_else(|| "NULL".into(), |n| n.to_string()),
            notes = opt_sql_literal(notes.as_deref()),
        );

        engine.execute_command(&insert_sql)?;
        Ok(())
    })?;
    Ok(())
}

/// Partial UPDATE of prose fields for one table. Errors with
/// [`ErrorCode::TableNotFound`] if there is no catalog row for
/// `table_name` (callers can decide whether to first stub via
/// [`upsert_stub`] or surface the error).
///
/// # Errors
///
/// - Returns [`ErrorCode::EmptyData`] if `fields` contains no values
///   to update.
/// - Returns [`ErrorCode::TableNotFound`] if no catalog row exists for
///   `table_name`.
/// - Propagates any error from [`ensure_exists`], [`get`], or the
///   `UPDATE` statement.
pub fn set_metadata(
    engine: &Engine,
    table_name: &str,
    fields: &MetadataFields,
) -> Result<CatalogEntry, McpError> {
    ensure_exists(engine)?;

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
    // stub row.
    let existing = get(engine, table_name)?.ok_or_else(|| {
        McpError::new(
            ErrorCode::TableNotFound,
            format!(
                "No catalog entry for table '{table_name}'. Load the table first \
                 (load_file / load_data / execute CREATE TABLE) or create it and \
                 re-run; the catalog is refreshed automatically on those paths."
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

    let update_sql = format!(
        "UPDATE \"{TABLE_CATALOG_TABLE}\" SET {assigns} WHERE table_name = {name}",
        assigns = assignments.join(", "),
        name = sql_literal(table_name),
    );
    engine.execute_command(&update_sql)?;

    // Re-read so the caller gets the canonical view (including unchanged
    // fields). `existing` is our fallback if the re-read somehow returns
    // nothing — that shouldn't happen, but preserves the previous row
    // instead of failing spuriously.
    get(engine, table_name)?.map_or(Ok(existing), Ok)
}

/// Delete the catalog row (if any) for `table_name`. Called when a table
/// is dropped. Idempotent — no error if the row doesn't exist.
///
/// # Errors
///
/// Propagates any error from `table_present` or from the `DELETE`
/// statement against the catalog table.
pub fn delete_for(engine: &Engine, table_name: &str) -> Result<bool, McpError> {
    if !table_present(engine)? {
        return Ok(false);
    }
    let sql = format!(
        "DELETE FROM \"{TABLE_CATALOG_TABLE}\" WHERE table_name = {}",
        sql_literal(table_name)
    );
    let affected = engine.execute_command(&sql)?;
    Ok(affected > 0)
}

/// Synchronize the catalog against the current set of user tables.
///
/// * Insert a stub row for every user table missing from the catalog.
///   These stubs use `load_tool = "unknown"` so callers can later tell
///   the difference between "loaded via a tool and we tracked it" and
///   "found during reconciliation".
/// * Delete catalog rows whose table no longer exists in Hyper.
/// * Refresh `row_count` on every remaining row so `SELECT * FROM
///   _table_catalog` always reflects current size (cheap: it's one
///   `COUNT(*)` per table, and the user set is small).
///
/// Does *not* bump `last_refreshed_at` — reconciliation is a housekeeping
/// pass, not a data refresh. Only explicit loads mark a refresh.
///
/// # Errors
///
/// - Propagates any error from [`ensure_exists`], `user_tables`,
///   [`list`], [`delete_for`], `refresh_row_count`, or [`upsert_stub`].
/// - `row_count_of` failures are swallowed (the row count falls back
///   to `None`), so per-table count probe failures do not abort the
///   sweep.
pub fn reconcile(engine: &Engine) -> Result<(), McpError> {
    ensure_exists(engine)?;

    let tables = user_tables(engine)?;
    let catalog_entries = list(engine)?;
    let catalog_names: std::collections::HashSet<String> = catalog_entries
        .iter()
        .map(|e| e.table_name.clone())
        .collect();
    let live_tables: std::collections::HashSet<String> = tables.iter().cloned().collect();

    for entry in &catalog_entries {
        if !live_tables.contains(&entry.table_name) {
            delete_for(engine, &entry.table_name)?;
        }
    }

    for table in &tables {
        let row_count = row_count_of(engine, table).ok();
        if catalog_names.contains(table) {
            refresh_row_count(engine, table, row_count)?;
        } else {
            upsert_stub(engine, table, "unknown", None, row_count, false)?;
        }
    }
    Ok(())
}

// --- Internals --------------------------------------------------------------

/// List user-facing tables (excludes `_hyperdb_*` internals and the
/// catalog itself). Reads the raw table list from `Catalog` directly
/// rather than going through `describe_tables`, which already filters
/// out internal tables (including `_table_catalog` itself) and would
/// hide the rows we want to compare against.
fn user_tables(engine: &Engine) -> Result<Vec<String>, McpError> {
    let catalog = hyperdb_api::Catalog::new(engine.connection());
    let names = catalog.get_table_names("public").map_err(McpError::from)?;
    Ok(names
        .into_iter()
        .filter(|name| name != TABLE_CATALOG_TABLE && !is_internal_table(name))
        .collect())
}

/// `true` when `_table_catalog` is already present in the workspace.
/// Used by read paths to return an empty result instead of erroring on a
/// brand-new workspace where the table hasn't been created yet. Reads
/// the raw catalog directly so the result isn't affected by the
/// internal-table filter applied to user-facing `describe_tables`.
fn table_present(engine: &Engine) -> Result<bool, McpError> {
    let catalog = hyperdb_api::Catalog::new(engine.connection());
    let names = catalog.get_table_names("public").map_err(McpError::from)?;
    Ok(names.iter().any(|n| n == TABLE_CATALOG_TABLE))
}

/// Return `COUNT(*)` for a user table. Quoted to handle mixed-case or
/// keyword-like names. The `describe_tables` path already gives us a row
/// count, but only after a full schema read; this dedicated query is
/// cheaper when we only need the number.
fn row_count_of(engine: &Engine, table_name: &str) -> Result<i64, McpError> {
    let quoted = table_name.replace('"', "\"\"");
    let sql = format!("SELECT COUNT(*) AS cnt FROM \"{quoted}\"");
    let rows = engine.execute_query_to_json(&sql)?;
    Ok(rows
        .first()
        .and_then(|r| r.get("cnt").and_then(serde_json::Value::as_i64))
        .unwrap_or(0))
}

/// Cheap UPDATE for just the `row_count` column of an existing row.
/// Used by [`reconcile`] so we don't rewrite the whole row just to
/// refresh counts.
fn refresh_row_count(
    engine: &Engine,
    table_name: &str,
    row_count: Option<i64>,
) -> Result<(), McpError> {
    let sql = format!(
        "UPDATE \"{TABLE_CATALOG_TABLE}\" SET row_count = {count} WHERE table_name = {name}",
        count = row_count.map_or_else(|| "NULL".into(), |n| n.to_string()),
        name = sql_literal(table_name),
    );
    engine.execute_command(&sql)?;
    Ok(())
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
