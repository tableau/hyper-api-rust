// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Ingest inline data (JSON strings, CSV strings) and CSV files into Hyper tables.
//!
//! JSON is inserted row-by-row via SQL `INSERT` statements. This is simple and
//! correct but not the fastest path — for bulk data, prefer file-based ingest
//! where Hyper's native `COPY FROM` is used.
//!
//! CSV ingest always uses `COPY FROM`: inline CSV is spilled to a temp file first,
//! while file-based CSV is read directly by `hyperd`.
//!
//! # Atomicity
//!
//! Every ingest function wraps its `INSERT` / `COPY` work inside a single
//! transaction via [`Engine::execute_in_transaction`]. If any row fails to
//! insert, all prior inserts from the same call are rolled back, so a failed
//! ingest leaves zero additional rows behind.
//!
//! Note that Hyper auto-commits DDL (`DROP TABLE`, `CREATE TABLE`) regardless
//! of the surrounding transaction. In `replace` mode, this means the original
//! table is already gone by the time inserts start — a mid-ingest failure
//! leaves the new table empty, not the original intact. In `append` mode, no
//! DDL runs (assuming the table already exists), so rollback is fully atomic.

use crate::engine::Engine;
use crate::error::{ErrorCode, McpError};
use crate::schema::{
    apply_schema_override, infer_csv_schema, infer_json_schema, json_type_name,
    widen_csv_numeric_columns, ColumnSchema,
};
use crate::stats::{IngestStats, StatsTimer};
use hyperdb_api::AsyncConnection;
use std::path::{Path, PathBuf};

/// Resolve a path to a form that's safe to embed in a SQL
/// `COPY FROM` literal.
///
/// On macOS the temp dir lives under `/var`, which is a symlink to
/// `/private/var`; Hyper's `COPY FROM` resolves the symlink and then
/// opens the file by the resolved path, so passing the unresolved one
/// would break inline-CSV ingest. `canonicalize()` fixes that.
///
/// On Windows two extra steps are needed:
///
/// 1. `canonicalize()` returns an extended-length prefix (`\\?\C:\...`)
///    that Hyper's `COPY FROM` cannot parse — it returns SQLSTATE 55006
///    "unable to read from external source". Strip the prefix so the
///    path looks like a plain `C:\...`.
///
/// 2. Convert backslashes to forward slashes. `escape_string_literal`
///    only escapes single quotes, leaving `\U`, `\t`, and other
///    backslash sequences exposed to `PostgreSQL`'s legacy string-literal
///    escape rules, which Hyper inherits. Forward slashes are accepted
///    by the Win32 file APIs and sidestep the escape question entirely.
///
/// Any canonicalize failure falls back to the original path — the
/// common case (absolute, existing file on any platform) still works.
fn canonicalize_for_copy(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        if let Some(s) = canonical.to_str() {
            // `\\?\C:\foo\bar.csv` → `C:\foo\bar.csv`. Leave UNC-style
            // `\\?\UNC\server\share\...` alone — those are only produced
            // by canonicalize on UNC sources, which Hyper would reject
            // either way, and stripping blindly would corrupt them.
            let stripped = match s.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("UNC\\") => rest,
                _ => s,
            };
            return PathBuf::from(stripped.replace('\\', "/"));
        }
    }
    canonical
}
use serde_json::Value;

/// Maximum bytes read from a CSV/text file for schema inference (64 MB).
/// The full file is still loaded by `COPY FROM` — this limit only bounds the
/// in-process memory used for type detection.
const SCHEMA_INFERENCE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Reads at most `max_bytes` from a text file, returning a valid UTF-8 string.
/// Truncates at the last newline within the byte budget to avoid splitting a row
/// or multi-byte UTF-8 characters.
fn read_text_sample(path: impl AsRef<Path>, max_bytes: u64) -> std::io::Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path.as_ref())?;
    let file_len = file.metadata()?.len();
    if file_len <= max_bytes {
        return std::fs::read_to_string(path.as_ref());
    }
    let mut reader = std::io::BufReader::new(file);
    let cap = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let mut buf = vec![0u8; cap];
    reader.read_exact(&mut buf)?;
    // Truncate at last newline to avoid partial rows
    if let Some(pos) = buf.iter().rposition(|&b| b == b'\n') {
        buf.truncate(pos + 1);
    }
    // Handle UTF-8 boundary: if truncation split a multi-byte character,
    // trim trailing incomplete bytes until we have valid UTF-8
    match String::from_utf8(buf) {
        Ok(s) => Ok(s),
        Err(e) => {
            let valid_up_to = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid_up_to);
            // Truncate at last newline again to ensure we have complete rows
            if let Some(pos) = bytes.iter().rposition(|&b| b == b'\n') {
                bytes.truncate(pos + 1);
            }
            String::from_utf8(bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }
    }
}

/// Controls how data is loaded into a target table.
#[derive(Debug)]
pub struct IngestOptions {
    pub table: String,
    /// `"replace"` drops the existing table first; `"append"` adds rows to
    /// it; `"merge"` upserts rows by [`Self::merge_key`].
    pub mode: String,
    /// When set, bypasses schema inference and uses these exact column types.
    pub schema_override: Option<serde_json::Map<String, Value>>,
    /// When `mode == "merge"`, the column(s) to match on for upsert. Required
    /// in merge mode; rejected in any other mode (the per-call site validates
    /// up-front so the lower ingest paths can stay format-agnostic).
    pub merge_key: Option<Vec<String>>,
    /// Resolved database alias for fully-qualified SQL. `None` means the
    /// primary (ephemeral); `Some("persistent")` or `Some("user_alias")`
    /// qualifies table references as `"<db>"."public"."<table>"`.
    /// Must be pre-resolved via `Engine::resolve_target_db` before setting.
    pub target_db: Option<String>,
}

/// Build a SQL table identifier from `IngestOptions`. When `target_db` is
/// set, returns `"db"."public"."table"`; otherwise `"table"` (unqualified).
pub fn qualified_table(opts: &IngestOptions) -> String {
    match &opts.target_db {
        Some(db) => {
            let esc_db = db.replace('"', "\"\"");
            let esc_tbl = opts.table.replace('"', "\"\"");
            format!("\"{esc_db}\".\"public\".\"{esc_tbl}\"")
        }
        None => format!("\"{}\"", opts.table.replace('"', "\"\"")),
    }
}

/// Returned by every ingest function with the row count, resolved schema,
/// and performance telemetry.
#[derive(Debug)]
pub struct IngestResult {
    pub rows: u64,
    pub schema: Vec<ColumnSchema>,
    pub stats: IngestStats,
}

/// RAII guard that ensures a temp table is dropped on **every** scope
/// exit — `Ok`, `Err`, *and* panic-unwind. Used by
/// [`merge_via_temp_table`] so an orphan `__hyperdb_merge_*` table
/// can't leak into the workspace, even if a per-format ingest path
/// panics mid-load.
///
/// Disarm by calling [`TempTableGuard::disarm`] when the temp has
/// been renamed away (so the guard isn't dropping a real
/// user-facing table by name).
struct TempTableGuard<'a> {
    engine: &'a Engine,
    name: String,
    /// Target database the temp lives in. `None` → primary (unqualified
    /// drop). `Some(alias)` → emit `"alias"."public"."<name>"` so the
    /// drop lands in the same DB the temp was created in.
    target_db: Option<String>,
    armed: bool,
}

impl<'a> TempTableGuard<'a> {
    fn new(engine: &'a Engine, name: String, target_db: Option<String>) -> Self {
        Self {
            engine,
            name,
            target_db,
            armed: true,
        }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempTableGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Detect whether the guard is being dropped during normal
        // unwinding vs. as part of a panic. Including this in the log
        // lets ops distinguish "expected cleanup" from "we wouldn't
        // have learned about the temp leak otherwise" — the latter is
        // the load-bearing case for this guard's existence.
        let panicking = std::thread::panicking();
        let quoted = match &self.target_db {
            Some(db) => format!(
                "\"{}\".\"public\".\"{}\"",
                db.replace('"', "\"\""),
                self.name.replace('"', "\"\""),
            ),
            None => format!("\"{}\"", self.name.replace('"', "\"\"")),
        };
        let drop_sql = format!("DROP TABLE IF EXISTS {quoted}");
        match self.engine.execute_command(&drop_sql) {
            Ok(_) => {
                if panicking {
                    // Successful cleanup during a panic-unwind is
                    // exactly what the guard exists for. Note it at
                    // info so it shows up in normal log review without
                    // requiring debug-level capture.
                    tracing::info!(
                        tmp = %self.name,
                        "merge_via_temp_table: dropped temp table during panic unwind"
                    );
                }
            }
            Err(e) => {
                // Don't mask the original outcome's error with a
                // cleanup error — log and continue. The temp table is
                // benign; the user can drop it manually if it ever
                // surfaces. `panicking` is included so ops can tell
                // whether this was a normal-path cleanup failure or
                // happened while another error was already
                // propagating.
                tracing::warn!(
                    tmp = %self.name,
                    panicking,
                    error = %e,
                    "merge_via_temp_table: failed to drop temp table on guard exit"
                );
            }
        }
    }
}

/// Implement `mode = "merge"` for any format by reusing that format's
/// `replace`-mode load to populate a temp table, then upserting the
/// target by `merge_key` columns.
///
/// `replace_load` is the format-specific entry point (e.g. `ingest_json`,
/// `ingest_parquet_file`) called with a `replace`-mode [`IngestOptions`]
/// pointing at a temp table. It must return the [`IngestResult`] for
/// the temp-table load — caller stitches that into the final result.
///
/// Algorithm:
///
/// 1. Validate `merge_key` is set and non-empty.
/// 2. Load incoming data into a unique temp table via `replace_load`.
///    A `TempTableGuard` arms here and unwind-safely drops the temp
///    on every exit (`Ok` / `Err` / panic). Verify the temp actually
///    materialized — a no-op load would otherwise produce confusing
///    downstream errors.
/// 3. If target table doesn't exist, rename the temp table → target,
///    disarm the guard (the temp is now the target, do **not** drop
///    it), and return (degenerates to "create").
/// 4. Read target + temp column metadata. Validate every merge key
///    exists in both with `types_compatible` type. Reject any
///    non-key shared column whose type differs.
/// 5. Auto-`ALTER TABLE ADD COLUMN` for any column present in temp but
///    not target (added as nullable). When this fires, set
///    [`IngestStats::schema_changed`] so the caller can issue a
///    resource-list-changed notification.
/// 6. `DELETE FROM target USING temp WHERE <key match>` then
///    `INSERT INTO target (<temp cols>) SELECT <temp cols> FROM temp`.
///    Columns the target has but temp doesn't are deliberately omitted
///    from the projection so they fall through to NULL — standard
///    PostgreSQL semantics for partial inserts.
/// 7. The `TempTableGuard` drops the temp on scope exit.
///
/// Atomicity: the DELETE+INSERT pair is **not** wrapped in a transaction.
/// Hyper auto-commits DDL inside transactions (see module-level note),
/// and the existing `replace` mode already accepts the same race window,
/// so this matches precedent rather than introducing a new exposure.
///
/// # Errors
///
/// - [`ErrorCode::InvalidArgument`] when `merge_key` is missing/empty,
///   or when a key column is missing from target or temp, or when a
///   column type differs between target and temp on a shared column.
/// - [`ErrorCode::InternalError`] if `replace_load` returns `Ok` but
///   doesn't actually create the temp table (a contract violation).
/// - Propagates errors from `replace_load`, [`Engine::column_metadata`],
///   [`Engine::alter_table_add_columns`], or the underlying DELETE /
///   INSERT statements. The temp table is dropped before the error
///   propagates.
pub fn merge_via_temp_table<F>(
    engine: &Engine,
    opts: &IngestOptions,
    replace_load: F,
) -> Result<IngestResult, McpError>
where
    F: FnOnce(&IngestOptions) -> Result<IngestResult, McpError>,
{
    // Belt-and-suspenders contract check. Every per-format ingest only
    // calls this helper when `opts.mode == "merge"`; if a future
    // refactor adds a third mode that also recurses, this debug-only
    // assertion catches the mistake before it manifests as silent
    // misbehavior. The closure also passes `mode = "replace"` to the
    // recursed call, breaking infinite recursion.
    debug_assert_eq!(
        opts.mode, "merge",
        "merge_via_temp_table called with non-merge mode `{}`",
        opts.mode
    );

    // Step 1 — validate merge_key.
    let keys = opts.merge_key.as_ref().ok_or_else(|| {
        McpError::new(
            ErrorCode::InvalidArgument,
            "merge mode requires merge_key (a column name or list of column names)",
        )
    })?;
    if keys.is_empty() || keys.iter().any(String::is_empty) {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            "merge_key must be a non-empty list of non-empty column names",
        ));
    }

    // Step 2 — load incoming data into a per-call temp table. The name
    // mixes PID + nanosecond timestamp + a process-local atomic counter.
    // The counter is what makes this race-free: two parallel merges of
    // the same target inside one process can land in the same nanosecond
    // (sub-µs OS timer, or platform-error fallback to `0`), but the
    // counter is monotonically unique per call. That's enough to keep
    // each merge's temp table distinct so the `TempTableGuard`s don't
    // cross-drop one another.
    use std::sync::atomic::{AtomicU64, Ordering};
    static MERGE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = MERGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // Squash anything that isn't [A-Za-z0-9_] in the target name to `_`
    // so the auto-generated identifier is always SQL-safe. The temp
    // name is never user-visible after a successful merge.
    let safe_target: String = opts
        .table
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let tmp = format!(
        "__hyperdb_merge_{safe_target}_{}_{nanos}_{counter}",
        std::process::id(),
    );

    // The temp table lives in the same database as the target. That
    // way every DML statement below (CREATE TABLE AS, DELETE-USING,
    // INSERT-SELECT) stays within a single DB — no cross-DB DML — and
    // the merge works the same regardless of whether the target is
    // primary, persistent, or any user-attached writable database.
    let tmp_opts = IngestOptions {
        table: tmp.clone(),
        mode: "replace".into(),
        schema_override: opts.schema_override.clone(),
        merge_key: None,
        target_db: opts.target_db.clone(),
    };
    let tmp_result = replace_load(&tmp_opts)?;

    // Arm the cleanup guard immediately after the load so any later
    // failure (or panic) drops the temp table on unwind. The guard
    // tracks `target_db` so the DROP lands in the same DB the temp
    // was created in.
    let mut guard = TempTableGuard::new(engine, tmp.clone(), opts.target_db.clone());

    // Belt-and-suspenders check: a per-format `replace_load` that returns
    // `Ok` without actually creating the table would surface as opaque
    // "table not found" errors from the catalog read in step 4. Catch
    // it here with a clear message so the contract violation is named
    // outright.
    if !engine.table_exists_in(opts.target_db.as_deref(), &tmp)? {
        return Err(McpError::new(
            ErrorCode::InternalError,
            format!(
                "merge: temp table '{tmp}' was not produced by the format-specific \
                 replace load — this is a contract violation in the per-format ingest path"
            ),
        ));
    }

    // Step 3 — if target doesn't exist, rename temp → target.
    if !engine.table_exists_in(opts.target_db.as_deref(), &opts.table)? {
        let qualified_tmp_opts = IngestOptions {
            table: tmp.clone(),
            mode: "replace".into(),
            schema_override: None,
            merge_key: None,
            target_db: opts.target_db.clone(),
        };
        let quoted_tmp = qualified_table(&qualified_tmp_opts);
        // RENAME TO accepts an unqualified new name (Hyper / PostgreSQL
        // semantics — the schema/database stays the same as the source).
        let escaped_new = opts.table.replace('"', "\"\"");
        engine.execute_command(&format!(
            "ALTER TABLE {quoted_tmp} RENAME TO \"{escaped_new}\""
        ))?;
        // The temp is now the target; the guard must NOT try to drop it.
        guard.disarm();
        return Ok(IngestResult {
            rows: tmp_result.rows,
            schema: tmp_result.schema,
            stats: IngestStats {
                operation: tmp_result.stats.operation,
                rows: tmp_result.stats.rows,
                elapsed_ms: tmp_result.stats.elapsed_ms,
                bytes_read: tmp_result.stats.bytes_read,
                bytes_stored: tmp_result.stats.bytes_stored,
                schema_inference_ms: tmp_result.stats.schema_inference_ms,
                table: opts.table.clone(),
                file_format: tmp_result.stats.file_format,
                warning: tmp_result.stats.warning,
                // Target was just created from scratch — by definition
                // the resource list "changed" relative to its prior
                // absence, so signal a notify.
                schema_changed: true,
            },
        });
    }

    // Step 4 — schema reconciliation. Read both schemas from the
    // target DB; in cross-DB merges the connection-bound `Catalog`
    // wouldn't see the attached database, so route via
    // `column_metadata_in` which falls back to the qualified
    // `pg_catalog.pg_attribute` probe.
    let target_cols = engine.column_metadata_in(opts.target_db.as_deref(), &opts.table)?;
    let tmp_cols = engine.column_metadata_in(opts.target_db.as_deref(), &tmp)?;
    // Every key must exist in both, with matching type.
    for k in keys {
        let in_target = target_cols.iter().find(|c| c.name == *k);
        let in_tmp = tmp_cols.iter().find(|c| c.name == *k);
        match (in_target, in_tmp) {
            (None, _) => {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "merge_key column '{k}' is not in target table '{}'",
                        opts.table
                    ),
                ));
            }
            (_, None) => {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "merge_key column '{k}' is not in incoming data \
                         (column missing from the file)"
                    ),
                ));
            }
            (Some(t), Some(s)) if !types_compatible(&t.hyper_type, &s.hyper_type) => {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "merge_key column '{k}' type mismatch: target is {} but \
                         incoming is {}. Use mode=replace or apply a schema override.",
                        t.hyper_type, s.hyper_type
                    ),
                ));
            }
            _ => {}
        }
    }
    // Reject type mismatches on any shared non-key column.
    for tc in &tmp_cols {
        if let Some(target_c) = target_cols.iter().find(|c| c.name == tc.name) {
            if !types_compatible(&target_c.hyper_type, &tc.hyper_type) {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "Column '{}' type mismatch: target is {} but incoming is {}. \
                         Use mode=replace or apply a schema override.",
                        tc.name, target_c.hyper_type, tc.hyper_type
                    ),
                ));
            }
        }
    }

    // Step 5 — auto-ALTER for new columns (in temp, not in target).
    // `alter_table_add_columns` handles the empty-input case by
    // returning early without issuing SQL.
    let new_cols: Vec<ColumnSchema> = tmp_cols
        .iter()
        .filter(|c| !target_cols.iter().any(|t| t.name == c.name))
        .cloned()
        // ALTER TABLE ADD COLUMN must be nullable; existing rows have no value.
        .map(|mut c| {
            c.nullable = true;
            c
        })
        .collect();
    let schema_changed = !new_cols.is_empty();
    engine.alter_table_add_columns_in(opts.target_db.as_deref(), &opts.table, &new_cols)?;

    // Step 6 — DELETE matching rows by key, then INSERT all temp rows.
    // Both target and temp share `opts.target_db`, so qualifying via
    // `qualified_table` keeps the DML scoped to one DB.
    let quoted_tgt = qualified_table(opts);
    let qualified_tmp_opts = IngestOptions {
        table: tmp.clone(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: opts.target_db.clone(),
    };
    let quoted_tmp = qualified_table(&qualified_tmp_opts);
    let key_eq = keys
        .iter()
        .map(|k| {
            let qk = k.replace('"', "\"\"");
            format!("t.\"{qk}\" = s.\"{qk}\"")
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let delete_sql = format!("DELETE FROM {quoted_tgt} t USING {quoted_tmp} s WHERE {key_eq}");
    engine.execute_command(&delete_sql)?;

    // The INSERT projection is `tmp_cols` (not `target_cols`). Columns
    // the target has but the incoming temp lacks are deliberately
    // omitted — PostgreSQL fills them with NULL, which is the right
    // widening semantic for an upsert that adds new rows for
    // unmatched keys.
    let cols_csv = tmp_cols
        .iter()
        .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let insert_sql =
        format!("INSERT INTO {quoted_tgt} ({cols_csv}) SELECT {cols_csv} FROM {quoted_tmp}");
    let inserted = engine.execute_command(&insert_sql)?;

    // Re-read target schema for the result so callers see the post-merge shape.
    let final_schema = engine.column_metadata_in(opts.target_db.as_deref(), &opts.table)?;

    // The guard drops the temp table when it falls out of scope here.
    Ok(IngestResult {
        rows: inserted,
        schema: final_schema,
        stats: IngestStats {
            operation: tmp_result.stats.operation,
            rows: inserted,
            elapsed_ms: tmp_result.stats.elapsed_ms,
            bytes_read: tmp_result.stats.bytes_read,
            bytes_stored: tmp_result.stats.bytes_stored,
            schema_inference_ms: tmp_result.stats.schema_inference_ms,
            table: opts.table.clone(),
            file_format: tmp_result.stats.file_format,
            warning: tmp_result.stats.warning,
            schema_changed,
        },
    })
}

/// Compare two Hyper type strings for merge compatibility.
///
/// Raw string equality is wrong: the catalog canonicalizes types
/// (`INT` → `INTEGER`, `BYTES` → `BYTEA`, `NUMERIC(15,2)` → may
/// differ in spacing) while the inferred-incoming side carries
/// whatever the Rust inferrer emitted (`"INT"`, `"DOUBLE
/// PRECISION"`, `"NUMERIC(15, 2)"`). Comparing through
/// [`crate::schema::map_hyper_type`] collapses all known aliases
/// into a single [`SqlType`].
///
/// If *either* side fails to parse (returns `None`), we err on the
/// side of permissive: treat the pair as compatible and let Hyper
/// itself enforce at INSERT time. This avoids false-rejecting
/// types the Rust mapper doesn't know about (future Hyper types,
/// custom Hyper extensions, anything `map_hyper_type` hasn't been
/// taught about). Mismatches will still surface — just from
/// `INSERT` rather than from our pre-flight check.
fn types_compatible(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (
        crate::schema::map_hyper_type(a),
        crate::schema::map_hyper_type(b),
    ) {
        (Some(sa), Some(sb)) => sa == sb,
        _ => true,
    }
}

/// Ingest an inline JSON array of objects into a Hyper table.
///
/// Each object becomes one row. Missing keys are inserted as NULL.
/// Emits a warning when the input exceeds 50 MB — the caller should
/// prefer `load_file` with a Parquet or Arrow file for large datasets.
///
/// # Errors
///
/// - Returns [`ErrorCode::SchemaMismatch`] if `json_str` cannot be
///   parsed as a JSON array of objects (via [`serde_json::from_str`]).
/// - Returns [`ErrorCode::EmptyData`] if the parsed array is empty.
/// - Propagates any schema-inference error from [`infer_json_schema`]
///   or [`apply_schema_override`].
/// - Propagates any [`McpError`] from the wrapping transaction —
///   [`Engine::create_table`], the per-row `INSERT` statements, or
///   transaction commit/rollback failures.
pub fn ingest_json(
    engine: &Engine,
    json_str: &str,
    opts: &IngestOptions,
) -> Result<IngestResult, McpError> {
    if opts.mode == "merge" {
        return merge_via_temp_table(engine, opts, |tmp_opts| {
            ingest_json(engine, json_str, tmp_opts)
        });
    }
    let timer = StatsTimer::start();
    let bytes_read = json_str.len() as u64;

    // Schema
    let schema_timer = StatsTimer::start();
    let inferred = infer_json_schema(json_str)?;
    let columns = match &opts.schema_override {
        Some(s) => apply_schema_override(inferred, s)?,
        None => inferred,
    };
    let schema_ms = schema_timer.elapsed_ms();

    // Parse JSON
    let array: Vec<serde_json::Map<String, Value>> = serde_json::from_str(json_str)
        .map_err(|e| McpError::new(ErrorCode::SchemaMismatch, format!("Invalid JSON: {e}")))?;

    if array.is_empty() {
        return Err(McpError::new(ErrorCode::EmptyData, "JSON array is empty"));
    }

    // All mutations run inside a transaction so a partial failure leaves
    // zero side effects.
    let is_replace = opts.mode != "append";
    let qualified = qualified_table(opts);
    let row_count = engine.execute_in_transaction(|engine| {
        engine.create_table_in(&opts.table, &columns, is_replace, opts.target_db.as_deref())?;
        let mut row_count = 0u64;
        let col_names: Vec<String> = columns.iter().map(|c| format!("\"{}\"", c.name)).collect();
        for obj in &array {
            let values: Vec<String> = columns
                .iter()
                .map(|col| match obj.get(&col.name) {
                    None | Some(Value::Null) => "NULL".to_string(),
                    Some(Value::Bool(b)) => b.to_string(),
                    Some(Value::Number(n)) => n.to_string(),
                    Some(Value::String(s)) => format!("'{}'", s.replace('\'', "''")),
                    Some(other) => format!("'{}'", other.to_string().replace('\'', "''")),
                })
                .collect();

            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                qualified,
                col_names.join(", "),
                values.join(", ")
            );
            engine.execute_command(&sql)?;
            row_count += 1;
        }
        Ok(row_count)
    })?;

    let elapsed = timer.elapsed_ms();
    let stats = IngestStats {
        operation: "load_data".into(),
        rows: row_count,
        elapsed_ms: elapsed,
        bytes_read,
        bytes_stored: 0,
        schema_inference_ms: Some(schema_ms),
        table: opts.table.clone(),
        file_format: Some("json".into()),
        warning: if bytes_read > 50_000_000 {
            Some("Large inline data. Consider using load_file for better performance.".into())
        } else {
            None
        },
        schema_changed: false,
    };

    Ok(IngestResult {
        rows: row_count,
        schema: columns,
        stats,
    })
}

/// Ingest inline CSV text into a Hyper table.
///
/// The CSV is written to a temp file and loaded via `COPY FROM` so that Hyper's
/// native CSV parser handles escaping, quoting, and bulk loading. The temp file
/// is cleaned up after the COPY completes.
///
/// # Errors
///
/// - Returns [`ErrorCode::InternalError`] if the temp CSV file cannot
///   be written to the system temp directory.
/// - Propagates any error from [`infer_csv_schema`],
///   [`widen_csv_numeric_columns`], or [`apply_schema_override`].
/// - Propagates any transaction error from [`Engine::create_table`]
///   or the `COPY FROM` statement (SQL errors, schema mismatches,
///   connection loss).
pub fn ingest_csv(
    engine: &Engine,
    csv_text: &str,
    opts: &IngestOptions,
) -> Result<IngestResult, McpError> {
    if opts.mode == "merge" {
        return merge_via_temp_table(engine, opts, |tmp_opts| {
            ingest_csv(engine, csv_text, tmp_opts)
        });
    }
    let timer = StatsTimer::start();
    let bytes_read = csv_text.len() as u64;

    // Schema: infer from the 1 000-row sample, widen numeric columns against
    // the full CSV body to catch values hidden after the sample window, then
    // overlay any user-provided partial override.
    let schema_timer = StatsTimer::start();
    let mut inferred = infer_csv_schema(csv_text, true)?;
    widen_csv_numeric_columns(csv_text.as_bytes(), true, &mut inferred)?;
    let columns = match &opts.schema_override {
        Some(s) => apply_schema_override(inferred, s)?,
        None => inferred,
    };
    let schema_ms = schema_timer.elapsed_ms();

    // Write CSV to a temp file before starting the transaction (it's a pure
    // filesystem operation and doesn't need to be atomic with the DB work).
    //
    // Uses `tempfile::NamedTempFile` for OS-guaranteed unique paths — the
    // previous PID+nanosecond scheme could collide on macOS where timer
    // resolution is coarser, causing parallel tests to race on the same file.
    // `into_temp_path()` closes the file handle immediately while retaining
    // the auto-delete-on-drop guarantee. This is required on Windows where
    // `hyperd` cannot open a file held by another process.
    let temp_path = tempfile::Builder::new()
        .prefix("hyperdb_mcp_csv_")
        .suffix(".csv")
        .tempfile()
        .map_err(|e| {
            McpError::new(
                ErrorCode::InternalError,
                format!("Failed to create temp CSV file: {e}"),
            )
        })?
        .into_temp_path();
    std::fs::write(&temp_path, csv_text).map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("Failed to write temp CSV: {e}"),
        )
    })?;

    // `NULL ''` makes unquoted empty CSV cells load as SQL NULL, which is
    // what users expect from the `,,` convention (and what `inspect_file`
    // already reports in its `null_count` diagnostics). Without this,
    // Hyper's CSV COPY treats empty cells as literal empty strings —
    // breaking downstream `WHERE col IS NULL` and failing outright on
    // numeric columns.
    let canonical_temp = canonicalize_for_copy(&temp_path);
    let qualified = qualified_table(opts);
    let copy_sql = format!(
        "COPY {} FROM {} WITH (FORMAT csv, NULL '', DELIMITER ',', HEADER)",
        qualified,
        hyperdb_api::escape_string_literal(canonical_temp.to_str().unwrap_or(""))
    );

    // Create table + COPY inside one transaction so that a COPY failure also
    // unwinds the table creation.
    let is_replace = opts.mode != "append";
    let row_count = engine.execute_in_transaction(|engine| {
        engine.create_table_in(&opts.table, &columns, is_replace, opts.target_db.as_deref())?;
        engine.execute_command(&copy_sql)
    });

    // `temp_path` (TempPath) auto-deletes the file when dropped at end of scope.
    drop(temp_path);
    let row_count = row_count?;

    let elapsed = timer.elapsed_ms();
    let stats = IngestStats {
        operation: "load_data".into(),
        rows: row_count,
        elapsed_ms: elapsed,
        bytes_read,
        bytes_stored: 0,
        schema_inference_ms: Some(schema_ms),
        table: opts.table.clone(),
        file_format: Some("csv".into()),
        warning: if bytes_read > 50_000_000 {
            Some("Large inline data. Consider using load_file for better performance.".into())
        } else {
            None
        },
        schema_changed: false,
    };

    Ok(IngestResult {
        rows: row_count,
        schema: columns,
        stats,
    })
}

/// Ingest a CSV file from disk into a Hyper table.
///
/// The file is read once for schema inference (up to 1 000 rows sampled),
/// then loaded in bulk via `COPY FROM` using the canonicalized path so
/// `hyperd` can read it directly.
///
/// # Errors
///
/// - Returns [`ErrorCode::FileNotFound`] if `path` does not exist or
///   cannot be read.
/// - Propagates any error from [`infer_csv_schema`],
///   [`widen_csv_numeric_columns`], or [`apply_schema_override`].
/// - Propagates any transaction error from [`Engine::create_table`]
///   or the `COPY FROM` statement.
pub fn ingest_csv_file(
    engine: &Engine,
    path: &str,
    opts: &IngestOptions,
) -> Result<IngestResult, McpError> {
    if opts.mode == "merge" {
        return merge_via_temp_table(engine, opts, |tmp_opts| {
            ingest_csv_file(engine, path, tmp_opts)
        });
    }
    let timer = StatsTimer::start();

    let abs_path = std::path::Path::new(path);
    if !abs_path.exists() {
        return Err(McpError::new(
            ErrorCode::FileNotFound,
            format!("File not found: {path}"),
        ));
    }

    let file_size = std::fs::metadata(abs_path).map_or(0, |m| m.len());

    // Read file for schema inference (capped at 64 MB to prevent OOM on huge files)
    let schema_timer = StatsTimer::start();
    let sample = read_text_sample(abs_path, SCHEMA_INFERENCE_MAX_BYTES)
        .map_err(|e| McpError::new(ErrorCode::FileNotFound, format!("Cannot read file: {e}")))?;
    let mut inferred = infer_csv_schema(&sample, true)?;
    widen_csv_numeric_columns(sample.as_bytes(), true, &mut inferred)?;
    let columns = match &opts.schema_override {
        Some(s) => apply_schema_override(inferred, s)?,
        None => inferred,
    };
    let schema_ms = schema_timer.elapsed_ms();

    // COPY FROM the file directly, inside a transaction with CREATE TABLE.
    let canonical = canonicalize_for_copy(abs_path);
    let qualified = qualified_table(opts);
    // See `ingest_csv` above for the NULL-handling rationale: `NULL ''`
    // makes unquoted empty cells load as SQL NULL.
    let copy_sql = format!(
        "COPY {} FROM {} WITH (FORMAT csv, NULL '', DELIMITER ',', HEADER)",
        qualified,
        hyperdb_api::escape_string_literal(canonical.to_str().unwrap_or(""))
    );

    let is_replace = opts.mode != "append";
    let row_count = engine.execute_in_transaction(|engine| {
        engine.create_table_in(&opts.table, &columns, is_replace, opts.target_db.as_deref())?;
        engine.execute_command(&copy_sql)
    })?;

    let elapsed = timer.elapsed_ms();
    let stats = IngestStats {
        operation: "load_file".into(),
        rows: row_count,
        elapsed_ms: elapsed,
        bytes_read: file_size,
        bytes_stored: 0,
        schema_inference_ms: Some(schema_ms),
        table: opts.table.clone(),
        file_format: Some("csv".into()),
        warning: None,
        schema_changed: false,
    };

    Ok(IngestResult {
        rows: row_count,
        schema: columns,
        stats,
    })
}

/// Async twin of [`ingest_csv_file`]. Runs schema inference on the
/// blocking pool, then issues `CREATE TABLE` + `COPY FROM` on the given
/// async connection inside a single transaction.
///
/// # Errors
///
/// - Returns [`ErrorCode::FileNotFound`] if `path` does not exist or
///   cannot be read.
/// - Returns [`ErrorCode::InternalError`] if the schema-inference task
///   panics on the blocking pool (surfaced as a join error).
/// - Propagates any error from schema inference or override
///   application.
/// - Propagates any transaction, `CREATE TABLE`, or `COPY FROM` error
///   from the async connection. A rollback failure after an inner
///   error is logged but does not override the original error.
pub async fn ingest_csv_file_async(
    conn: &AsyncConnection,
    path: &str,
    opts: &IngestOptions,
) -> Result<IngestResult, McpError> {
    let timer = StatsTimer::start();

    let abs_path = std::path::Path::new(path);
    if !abs_path.exists() {
        return Err(McpError::new(
            ErrorCode::FileNotFound,
            format!("File not found: {path}"),
        ));
    }
    let file_size = std::fs::metadata(abs_path).map_or(0, |m| m.len());

    // Inference is CPU-bound (parses the whole file to widen numerics)
    // so it runs on the blocking pool rather than stalling a worker.
    let schema_timer = StatsTimer::start();
    let path_owned = path.to_string();
    let override_owned = opts.schema_override.clone();
    let columns: Vec<ColumnSchema> = tokio::task::spawn_blocking(move || -> Result<_, McpError> {
        let sample = read_text_sample(&path_owned, SCHEMA_INFERENCE_MAX_BYTES).map_err(|e| {
            McpError::new(ErrorCode::FileNotFound, format!("Cannot read file: {e}"))
        })?;
        let mut inferred = infer_csv_schema(&sample, true)?;
        widen_csv_numeric_columns(sample.as_bytes(), true, &mut inferred)?;
        let columns = match &override_owned {
            Some(s) => apply_schema_override(inferred, s)?,
            None => inferred,
        };
        Ok(columns)
    })
    .await
    .map_err(|e| McpError::new(ErrorCode::InternalError, format!("Task join error: {e}")))??;
    let schema_ms = schema_timer.elapsed_ms();

    let canonical = canonicalize_for_copy(abs_path);
    let qualified = qualified_table(opts);
    // `NULL ''` — unquoted empty cells load as SQL NULL (see sync twin).
    let copy_sql = format!(
        "COPY {} FROM {} WITH (FORMAT csv, NULL '', DELIMITER ',', HEADER)",
        qualified,
        hyperdb_api::escape_string_literal(canonical.to_str().unwrap_or(""))
    );

    let is_replace = opts.mode != "append";

    conn.begin_transaction().await.map_err(McpError::from)?;
    let inner: Result<u64, McpError> = async {
        create_table_async(
            conn,
            &opts.table,
            &columns,
            is_replace,
            opts.target_db.as_deref(),
        )
        .await?;
        conn.execute_command(&copy_sql)
            .await
            .map_err(McpError::from)
    }
    .await;
    let row_count = match inner {
        Ok(n) => {
            conn.commit().await.map_err(McpError::from)?;
            n
        }
        Err(e) => {
            if let Err(rb) = conn.rollback().await {
                tracing::warn!("rollback after error failed: {}", rb);
            }
            return Err(e);
        }
    };

    let elapsed = timer.elapsed_ms();
    let stats = IngestStats {
        operation: "load_file".into(),
        rows: row_count,
        elapsed_ms: elapsed,
        bytes_read: file_size,
        bytes_stored: 0,
        schema_inference_ms: Some(schema_ms),
        table: opts.table.clone(),
        file_format: Some("csv".into()),
        warning: None,
        schema_changed: false,
    };

    Ok(IngestResult {
        rows: row_count,
        schema: columns,
        stats,
    })
}

/// Ingest a JSON or JSON-Lines file from disk into a Hyper table.
///
/// Auto-detects the payload shape from the first non-whitespace byte:
///
/// * `[` → a single JSON array of objects, loaded verbatim via
///   [`ingest_json`].
/// * anything else → newline-delimited JSON (`.jsonl` convention): each
///   non-empty line is parsed as one JSON object, collected into an
///   array, then passed to [`ingest_json`].
///
/// The dispatch is content-based rather than extension-based so `.json`
/// files that happen to contain JSONL (or vice-versa) still load; the
/// extension `.jsonl` is accepted but not required. Blank lines and
/// lines containing only whitespace are skipped.
///
/// # Errors
///
/// - Returns [`ErrorCode::FileNotFound`] if `path` does not exist or
///   cannot be read.
/// - Propagates errors from [`normalize_json_or_jsonl`] (malformed
///   JSON / JSONL) and from [`ingest_json`] (schema inference,
///   transaction failures, etc.).
pub fn ingest_json_file(
    engine: &Engine,
    path: &str,
    opts: &IngestOptions,
) -> Result<IngestResult, McpError> {
    let abs_path = std::path::Path::new(path);
    if !abs_path.exists() {
        return Err(McpError::new(
            ErrorCode::FileNotFound,
            format!("File not found: {path}"),
        ));
    }

    let text = std::fs::read_to_string(abs_path)
        .map_err(|e| McpError::new(ErrorCode::FileNotFound, format!("Cannot read file: {e}")))?;

    let json_array_text = normalize_json_or_jsonl(&text)?;
    let mut result = ingest_json(engine, &json_array_text, opts)?;

    // Re-label the operation + file_format so telemetry reflects the
    // file-based path; `ingest_json` defaults to inline-mode metadata.
    result.stats.operation = "load_file".into();
    // Preserve the actual on-disk size rather than the serialized array,
    // which may differ slightly after normalization.
    result.stats.bytes_read = std::fs::metadata(abs_path).map_or(0, |m| m.len());
    // Report the effective format we dispatched on so LLMs can
    // distinguish the JSONL path from the array path when debugging.
    result.stats.file_format = Some(
        if text.trim_start().starts_with('[') {
            "json"
        } else {
            "jsonl"
        }
        .into(),
    );

    Ok(result)
}

/// Async twin of [`ingest_json`]: insert a JSON array of objects on the
/// given async connection. See [`ingest_json`] for the JSON-shape
/// contract (expects a top-level array; call [`normalize_json_or_jsonl`]
/// first if you have JSONL).
///
/// # Errors
///
/// - Returns [`ErrorCode::SchemaMismatch`] if `json_str` cannot be
///   parsed as a JSON array of objects.
/// - Returns [`ErrorCode::EmptyData`] if the parsed array is empty.
/// - Propagates any error from [`infer_json_schema`] /
///   [`apply_schema_override`].
/// - Propagates any transaction or `INSERT`-loop error from the async
///   connection. Rollback failures after an inner error are logged
///   but do not shadow the original error.
pub async fn ingest_json_async(
    conn: &AsyncConnection,
    json_str: &str,
    opts: &IngestOptions,
) -> Result<IngestResult, McpError> {
    let timer = StatsTimer::start();
    let bytes_read = json_str.len() as u64;

    let schema_timer = StatsTimer::start();
    let inferred = infer_json_schema(json_str)?;
    let columns = match &opts.schema_override {
        Some(s) => apply_schema_override(inferred, s)?,
        None => inferred,
    };
    let schema_ms = schema_timer.elapsed_ms();

    let array: Vec<serde_json::Map<String, Value>> = serde_json::from_str(json_str)
        .map_err(|e| McpError::new(ErrorCode::SchemaMismatch, format!("Invalid JSON: {e}")))?;
    if array.is_empty() {
        return Err(McpError::new(ErrorCode::EmptyData, "JSON array is empty"));
    }

    let is_replace = opts.mode != "append";
    let qualified = qualified_table(opts);

    conn.begin_transaction().await.map_err(McpError::from)?;
    let inner: Result<u64, McpError> = async {
        create_table_async(
            conn,
            &opts.table,
            &columns,
            is_replace,
            opts.target_db.as_deref(),
        )
        .await?;
        let mut row_count = 0u64;
        let col_names: Vec<String> = columns.iter().map(|c| format!("\"{}\"", c.name)).collect();
        for obj in &array {
            let values: Vec<String> = columns
                .iter()
                .map(|col| match obj.get(&col.name) {
                    None | Some(Value::Null) => "NULL".to_string(),
                    Some(Value::Bool(b)) => b.to_string(),
                    Some(Value::Number(n)) => n.to_string(),
                    Some(Value::String(s)) => format!("'{}'", s.replace('\'', "''")),
                    Some(other) => format!("'{}'", other.to_string().replace('\'', "''")),
                })
                .collect();

            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                qualified,
                col_names.join(", "),
                values.join(", ")
            );
            conn.execute_command(&sql).await.map_err(McpError::from)?;
            row_count += 1;
        }
        Ok(row_count)
    }
    .await;

    let row_count = match inner {
        Ok(n) => {
            conn.commit().await.map_err(McpError::from)?;
            n
        }
        Err(e) => {
            if let Err(rb) = conn.rollback().await {
                tracing::warn!("rollback after error failed: {}", rb);
            }
            return Err(e);
        }
    };

    let elapsed = timer.elapsed_ms();
    let stats = IngestStats {
        operation: "load_data".into(),
        rows: row_count,
        elapsed_ms: elapsed,
        bytes_read,
        bytes_stored: 0,
        schema_inference_ms: Some(schema_ms),
        table: opts.table.clone(),
        file_format: Some("json".into()),
        warning: if bytes_read > 50_000_000 {
            Some("Large inline data. Consider using load_file for better performance.".into())
        } else {
            None
        },
        schema_changed: false,
    };

    Ok(IngestResult {
        rows: row_count,
        schema: columns,
        stats,
    })
}

/// Async twin of [`ingest_json_file`].
///
/// # Errors
///
/// - Returns [`ErrorCode::FileNotFound`] if `path` does not exist or
///   cannot be read.
/// - Propagates errors from [`normalize_json_or_jsonl`] and from
///   [`ingest_json_async`].
pub async fn ingest_json_file_async(
    conn: &AsyncConnection,
    path: &str,
    opts: &IngestOptions,
) -> Result<IngestResult, McpError> {
    let abs_path = std::path::Path::new(path);
    if !abs_path.exists() {
        return Err(McpError::new(
            ErrorCode::FileNotFound,
            format!("File not found: {path}"),
        ));
    }

    let text = std::fs::read_to_string(abs_path)
        .map_err(|e| McpError::new(ErrorCode::FileNotFound, format!("Cannot read file: {e}")))?;

    let json_array_text = normalize_json_or_jsonl(&text)?;
    let mut result = ingest_json_async(conn, &json_array_text, opts).await?;

    result.stats.operation = "load_file".into();
    result.stats.bytes_read = std::fs::metadata(abs_path).map_or(0, |m| m.len());
    result.stats.file_format = Some(
        if text.trim_start().starts_with('[') {
            "json"
        } else {
            "jsonl"
        }
        .into(),
    );

    Ok(result)
}

/// Shared helper: `CREATE TABLE` (optionally dropping first) on an async
/// connection. Mirrors [`Engine::create_table`] exactly so the async
/// ingest paths produce identical tables to the sync ones. Callers that
/// need atomicity should wrap this in `begin_transaction` / `commit`.
pub(crate) async fn create_table_async(
    conn: &AsyncConnection,
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
        conn.execute_command(&format!("DROP TABLE IF EXISTS {quoted_table}"))
            .await
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
    conn.execute_command(&create_sql)
        .await
        .map_err(McpError::from)?;
    Ok(())
}

/// Normalize a raw JSON / JSONL string to the "JSON array of objects"
/// representation [`ingest_json`] expects. Returns the original string
/// when it already parses as a top-level array; otherwise treats the
/// input as JSONL and re-serializes into a JSON array.
///
/// Public so [`crate::inspect`] can reuse the same JSON/JSONL
/// auto-detection for its dry-run inspector.
///
/// # Errors
///
/// - Returns [`ErrorCode::SchemaMismatch`] with the offending line
///   number when a JSONL line fails to parse as valid JSON.
/// - Returns [`ErrorCode::EmptyData`] if the input contains no
///   non-blank records.
/// - Returns [`ErrorCode::InternalError`] if the aggregated array
///   cannot be serialized back to a string (should not happen in
///   practice).
pub fn normalize_json_or_jsonl(text: &str) -> Result<String, McpError> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        return Ok(text.to_string());
    }

    // JSONL path: one JSON object per non-empty line. We parse each line
    // eagerly rather than concatenating then parsing, so malformed lines
    // produce a useful per-line error pointing at the exact offender.
    let mut objects: Vec<Value> = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Invalid JSON on line {}: {e}", idx + 1),
            )
        })?;
        objects.push(value);
    }
    if objects.is_empty() {
        return Err(McpError::new(
            ErrorCode::EmptyData,
            "JSON/JSONL file contained no records",
        ));
    }
    serde_json::to_string(&Value::Array(objects)).map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("Failed to serialize JSONL as array: {e}"),
        )
    })
}

/// Navigate a dot-separated path into a JSON value, transparently parsing
/// stringified JSON at each step.
///
/// Path segments are dot-separated (e.g., `content.0.text.query_result.results`).
/// Numeric segments index into JSON arrays (`content.0` means `content[0]`).
/// When a segment resolves to a JSON string, the function automatically tries
/// to parse that string as JSON and continues navigating into the parsed
/// result. This handles the common MCP wrapper pattern where response
/// payloads are double-encoded as stringified JSON.
///
/// After all segments are consumed, if the final value is still a string,
/// one more parse attempt is made so that a terminal stringified array
/// (e.g., `"[{\"a\":1}]"`) is returned as the parsed array.
///
/// Returns the serialized JSON string of the value at the terminal path
/// segment.
///
/// # Errors
///
/// Returns [`ErrorCode::SchemaMismatch`] when:
/// - `raw_json` is not valid JSON.
/// - A stringified JSON value at a traversal point cannot be re-parsed.
/// - A numeric segment is out of range for the current array, or the
///   current value is not an array.
/// - A named segment is missing from the current object, or the current
///   value is not an object.
/// - The final result cannot be serialized back to a JSON string
///   (surfaces as [`ErrorCode::InternalError`]).
pub fn extract_json_path(raw_json: &str, path: &str) -> Result<String, McpError> {
    let mut current: Value = serde_json::from_str(raw_json).map_err(|e| {
        McpError::new(
            ErrorCode::SchemaMismatch,
            format!("json_extract_path: file is not valid JSON: {e}"),
        )
    })?;

    let segments: Vec<&str> = path.split('.').collect();
    let mut traversed: Vec<&str> = Vec::new();

    for segment in &segments {
        // If the current value is a string, try to parse it as JSON before
        // applying this segment. This handles stringified JSON wrappers.
        if let Value::String(s) = &current {
            current = serde_json::from_str(s).map_err(|_| {
                McpError::new(
                    ErrorCode::SchemaMismatch,
                    format!(
                        "json_extract_path '{}': at segment '{}' (after '{}'): \
                         value is a string but not valid JSON",
                        path,
                        segment,
                        traversed.join(".")
                    ),
                )
            })?;
        }

        current = if let Ok(idx) = segment.parse::<usize>() {
            // Numeric segment: index into array.
            match current {
                Value::Array(mut arr) => {
                    if idx >= arr.len() {
                        return Err(McpError::new(
                            ErrorCode::SchemaMismatch,
                            format!(
                                "json_extract_path '{}': at segment '{}' (after '{}'): \
                                 array index {} out of bounds (length {})",
                                path,
                                segment,
                                traversed.join("."),
                                idx,
                                arr.len()
                            ),
                        ));
                    }
                    arr.swap_remove(idx)
                }
                other => {
                    return Err(McpError::new(
                        ErrorCode::SchemaMismatch,
                        format!(
                            "json_extract_path '{}': at segment '{}' (after '{}'): \
                             expected array, found {}",
                            path,
                            segment,
                            traversed.join("."),
                            json_type_name(&other)
                        ),
                    ));
                }
            }
        } else {
            // String segment: index into object by key.
            match current {
                Value::Object(mut map) => match map.remove(*segment) {
                    Some(v) => v,
                    None => {
                        return Err(McpError::new(
                            ErrorCode::SchemaMismatch,
                            format!(
                                "json_extract_path '{}': at segment '{}' (after '{}'): \
                                 key not found in object",
                                path,
                                segment,
                                traversed.join(".")
                            ),
                        ));
                    }
                },
                other => {
                    return Err(McpError::new(
                        ErrorCode::SchemaMismatch,
                        format!(
                            "json_extract_path '{}': at segment '{}' (after '{}'): \
                             expected object, found {}",
                            path,
                            segment,
                            traversed.join("."),
                            json_type_name(&other)
                        ),
                    ));
                }
            }
        };

        traversed.push(segment);
    }

    // Terminal auto-parse: if the final value is a string, try to parse it
    // as JSON so that e.g. a stringified array becomes the actual array.
    if let Value::String(s) = &current {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            current = parsed;
        }
    }

    serde_json::to_string(&current).map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("json_extract_path: failed to serialize extracted value: {e}"),
        )
    })
}

/// High-level file-format categories the ingest and inspect layers
/// dispatch on. Text-based formats (JSON, CSV) are distinguished by
/// peeking at the first non-whitespace byte when the extension is
/// unfamiliar, so log-like files with `.log`/`.txt`/no extension still
/// reach the right decoder without the caller having to rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferredFileFormat {
    /// Columnar, binary. Matched by `.parquet` / `.pq` extensions.
    Parquet,
    /// Arrow IPC stream or file. Matched by `.arrow` / `.ipc` /
    /// `.feather` extensions.
    ArrowIpc,
    /// JSON — either a top-level array of objects or newline-delimited
    /// JSON. [`ingest_json_file`] auto-detects between the two shapes
    /// from the first non-whitespace byte.
    Json,
    /// Everything else: CSV / TSV / log text that the CSV COPY path
    /// can still make sense of.
    Csv,
}

/// Decide how to dispatch an ingest / inspect call for `path`.
///
/// Extension first (zero-IO, cheap): binary formats (`.parquet`, `.pq`,
/// `.arrow`, `.ipc`, `.feather`) always win by extension because the
/// file is a binary container whose magic bytes we'd have to unpack
/// anyway. Known text extensions (`.json`, `.jsonl`, `.ndjson`) map
/// straight to JSON without needing to read the file.
///
/// Otherwise — for unknown, ambiguous, or missing extensions (`.log`,
/// `.txt`, no extension at all) — peek at the first 4 KiB and return
/// [`InferredFileFormat::Json`] if the first non-whitespace byte is
/// `[` or `{`, else [`InferredFileFormat::Csv`]. This is the path that
/// lets hyperd's raw `.log` files load without renaming, since JSONL
/// lines always begin with `{`.
///
/// Returns [`InferredFileFormat::Csv`] if the file can't be opened;
/// the subsequent CSV ingest will surface a clearer error than a
/// format-detection failure would.
#[must_use]
pub fn detect_file_format(path: &std::path::Path) -> InferredFileFormat {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "parquet" | "pq" => return InferredFileFormat::Parquet,
        "arrow" | "ipc" | "feather" => return InferredFileFormat::ArrowIpc,
        "json" | "jsonl" | "ndjson" => return InferredFileFormat::Json,
        _ => {}
    }
    // Content-sniff fallback for anything else. We only need the first
    // non-whitespace byte; 4 KiB comfortably covers any realistic
    // leading-whitespace padding or BOM noise.
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open(path) {
        let mut buf = [0u8; 4096];
        if let Ok(n) = f.read(&mut buf) {
            for b in &buf[..n] {
                match b {
                    b' ' | b'\t' | b'\n' | b'\r' | 0xEF | 0xBB | 0xBF => {}
                    b'[' | b'{' => return InferredFileFormat::Json,
                    _ => return InferredFileFormat::Csv,
                }
            }
        }
    }
    InferredFileFormat::Csv
}

#[cfg(test)]
mod read_text_sample_tests {
    use super::read_text_sample;
    use std::io::Write;

    fn write_temp(name: &str, contents: &[u8]) -> tempfile::TempPath {
        let mut tmp = tempfile::NamedTempFile::new().expect("temp file");
        tmp.write_all(contents).unwrap();
        tmp.flush().unwrap();
        // Use the path directly so the file persists for the read.
        let _ = name;
        tmp.into_temp_path()
    }

    #[test]
    fn small_file_returns_full_contents() {
        let path = write_temp("small.csv", b"a,b,c\n1,2,3\n");
        let s = read_text_sample(&path, 1024 * 1024).unwrap();
        assert_eq!(s, "a,b,c\n1,2,3\n");
    }

    #[test]
    fn truncates_at_last_newline_within_budget() {
        // 100 bytes total, cap at 30 — should truncate at last newline before byte 30.
        use std::fmt::Write;
        let mut data = String::new();
        for i in 0..20 {
            writeln!(&mut data, "row-{i}").unwrap();
        }
        let path = write_temp("rows.csv", data.as_bytes());
        let s = read_text_sample(&path, 30).unwrap();
        // Result should end with newline (no partial row).
        assert!(s.ends_with('\n'));
        // Should be at most 30 bytes.
        assert!(s.len() <= 30);
    }

    #[test]
    fn handles_utf8_boundary_split() {
        // Construct a file where the byte budget falls in the middle of a
        // multi-byte UTF-8 character (4-byte emoji).
        let prefix = b"row1\n".to_vec();
        let mut data = prefix.clone();
        // 4-byte emoji: 🔥 = 0xF0 0x9F 0x94 0xA5
        data.extend_from_slice("🔥".as_bytes());
        // Total: 5 + 4 = 9 bytes
        let path = write_temp("emoji.csv", &data);
        // Cap at 8 bytes — splits the emoji at byte 8 (after F0 9F 94)
        let s = read_text_sample(&path, 8).unwrap();
        // Result must be valid UTF-8 (the emoji is dropped).
        // Should end with the prefix's newline, not partial bytes.
        assert!(s.ends_with('\n'));
        assert_eq!(s, "row1\n");
    }

    #[test]
    fn returns_full_buffer_when_no_newline_under_budget() {
        // Single CSV row, no terminator, fits under cap.
        let path = write_temp("noeol.csv", b"a,b,c");
        let s = read_text_sample(&path, 1024).unwrap();
        assert_eq!(s, "a,b,c");
    }

    #[test]
    fn handles_no_newline_in_first_max_bytes() {
        // File larger than cap, no newlines anywhere — degenerate case.
        // We accept this (returns the full max_bytes prefix, possibly truncated
        // at last UTF-8 boundary). Schema inference will likely fail downstream,
        // but read_text_sample itself must not panic or return garbage.
        let data = vec![b'a'; 2048];
        let path = write_temp("noeol-big.csv", &data);
        let s = read_text_sample(&path, 100).unwrap();
        // No newline → keeps all 100 bytes of ASCII 'a'.
        assert_eq!(s.len(), 100);
        assert!(s.chars().all(|c| c == 'a'));
    }
}
