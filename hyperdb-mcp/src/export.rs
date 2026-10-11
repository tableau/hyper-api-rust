// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Export query results or whole tables to files.
//!
//! All row-oriented formats (CSV, Parquet, Arrow IPC, Iceberg) go through
//! hyperd's native `COPY (query) TO 'path' WITH (format => '…')` writer.
//! The MCP only issues a single SQL statement — no Rust-side type
//! inference, no JSON intermediate, no in-memory buffering of the
//! output. Hyperd writes the file (or directory) directly to disk and
//! reports the written row count back as the statement's affected-rows.
//!
//! Supported formats:
//! - **CSV** — `format => 'csv', header => true`.
//! - **Parquet** — `format => 'parquet'`. Preserves NUMERIC precision,
//!   DATE/TIMESTAMP types, and column nullability. Order of magnitude
//!   faster than the previous JSON-mediated path.
//! - **Arrow IPC Stream** — `format => 'arrowstream'`. Bit-identical to
//!   what Hyper speaks on the wire for its binary Arrow protocol.
//! - **Iceberg** — `format => 'iceberg'`. Destination is a *directory*
//!   (the Iceberg table root with `metadata/` and `data/` subdirs);
//!   hyperd creates it. Round-trips cleanly with `load_iceberg`.
//! - **Hyper** — new `.hyper` file populated via `CREATE DATABASE` +
//!   `ATTACH DATABASE` + a constraint-preserving per-table copy, openable
//!   directly in Tableau Desktop. (Cannot use plain `std::fs::copy` because
//!   on Windows `hyperd` holds an exclusive lock on the workspace file.)

use crate::engine::{Engine, require_read_only_sql};
use crate::error::{ErrorCode, McpError};
use crate::stats::{ExportStats, StatsTimer};
use hyperdb_api::{CopyTableReport, escape_sql_path, escape_string_literal};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Specifies what to export and where.
///
/// For row-oriented formats (`csv`, `parquet`, `arrow_ipc`, `iceberg`)
/// provide `sql` or `table`; `sql` takes priority if both are set. For
/// `hyper` format both are ignored — every user table in the workspace
/// is copied into a new `.hyper` file.
#[derive(Debug, Default)]
pub struct ExportOptions {
    /// A SELECT query whose results will be exported. Ignored when
    /// `format = "hyper"`.
    pub sql: Option<String>,
    /// Table name — converted to `SELECT * FROM "<table>"` when `sql` is
    /// None. Ignored when `format = "hyper"`.
    pub table: Option<String>,
    /// Destination file path. Must be absolute.
    pub path: String,
    /// One of `"csv"`, `"parquet"`, `"arrow_ipc"`, `"iceberg"`, or
    /// `"hyper"`.
    pub format: String,
    /// Whether to replace an existing destination. When `false` and
    /// `path` already exists, [`export_to_file`] returns a
    /// [`ErrorCode::PermissionDenied`] error without touching it. When
    /// `true`, `iceberg` replaces only an Iceberg table directory and
    /// `hyper` only a Hyper database file.
    pub overwrite: bool,
    /// Files the export must never write to, compared by identity: the
    /// session's own databases and its attached files.
    pub protected_paths: Vec<PathBuf>,
    /// Extra options passed through verbatim into the `WITH (...)`
    /// clause of hyperd's `COPY TO`. Keys must match hyperd's own
    /// option names (e.g. `codec`, `rows_per_row_group`,
    /// `max_file_size`, `table_scheme`, `delimiter`, `header`). Values
    /// may be strings, booleans, or numbers; anything else is rejected.
    /// Ignored for `"hyper"` format (which is not a `COPY` at all).
    pub format_options: Option<Map<String, Value>>,
    /// Source database to snapshot when `format = "hyper"`. `None` →
    /// the primary (ephemeral) workspace; `Some("persistent")` or
    /// `Some("user_alias")` snapshots that attached database. Ignored
    /// for the row-oriented formats which already qualify via `sql`
    /// or via `scoped_search_path`.
    pub source_db: Option<String>,
}

/// Returned by [`export_to_file`] with the exported row count and telemetry.
#[derive(Debug)]
pub struct ExportResult {
    pub rows: u64,
    pub stats: ExportStats,
    /// Which source constraints the copy reproduced, for `format = "hyper"`.
    /// `None` for the row-oriented formats, which carry no schema at all.
    pub schema_fidelity: Option<CopyTableReport>,
}

impl ExportResult {
    /// Report the destination as the caller named it: the export wrote to
    /// its validated form, which is the same file.
    fn reported_at(mut self, path: &str) -> Self {
        path.clone_into(&mut self.stats.output_path);
        self
    }
}

/// Top-level export dispatcher. Resolves the source SQL, then delegates to
/// the format-specific exporter.
///
/// # Errors
///
/// - Returns [`ErrorCode::InvalidArgument`] if `opts.path` is relative,
///   has `..` components or is not UTF-8, if it is one of
///   `opts.protected_paths`, or if `opts.overwrite` would replace
///   something other than an Iceberg table (`iceberg`) or a Hyper database
///   file (`hyper`).
/// - Returns [`ErrorCode::PermissionDenied`] if `opts.path` already
///   exists and `opts.overwrite` is `false`.
/// - Returns [`ErrorCode::SqlError`] when neither `opts.sql` nor
///   `opts.table` is provided (for row-oriented formats), when `opts.sql`
///   is not a single read-only statement, or when Hyper cannot parse it on
///   its own.
/// - Returns [`ErrorCode::UnsupportedFormat`] when `opts.format` is
///   not one of `hyper`, `csv`, `parquet`, `arrow_ipc`, or `iceberg`.
/// - Propagates any format-specific error from the delegated exporter
///   (SQL execution, I/O, or format-option validation failures).
pub fn export_to_file(engine: &Engine, opts: &ExportOptions) -> Result<ExportResult, McpError> {
    let timer = StatsTimer::start();

    // Reject `..` components to prevent traversal attacks via LLM-generated paths.
    let path_obj = std::path::Path::new(&opts.path);
    if path_obj
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!(
                "Export path '{}' may not contain '..' components",
                opts.path
            ),
        ));
    }

    // Everything below checks and writes the validated path, which has
    // symlinks in its existing part resolved. hyperd gets it in the form
    // it can open (no `\\?\` prefix on Windows), and so does the file
    // system, so both name the same file.
    let dest = crate::attach::validate_output_path(&opts.path, "export")?;
    crate::attach::refuse_protected_destination(&dest, &opts.protected_paths, "export")?;
    let dest = crate::ingest::path_for_hyperd(dest);
    let path = dest.to_str().ok_or_else(|| {
        McpError::new(
            ErrorCode::InvalidArgument,
            format!("Export path '{}' is not valid UTF-8", dest.display()),
        )
    })?;

    // Refuse to clobber an existing destination when caller opted out of
    // overwrite. Done up-front (before SQL resolution or format dispatch)
    // so every format — including the file-copy hyper path and the
    // directory-based iceberg path — gets the same guarantee. A dangling
    // symlink counts as existing.
    let exists = std::fs::symlink_metadata(&dest).is_ok();
    if !opts.overwrite && exists {
        return Err(McpError::new(
            ErrorCode::PermissionDenied,
            format!(
                "Refusing to overwrite existing destination: {path} (pass overwrite=true to replace it)"
            ),
        ));
    }

    // `hyper` format is a whole-workspace file copy — neither `sql` nor
    // `table` is meaningful for it, so branch before the SQL-resolution
    // check that the row-oriented formats require.
    if opts.format == "hyper" {
        return export_hyper(engine, path, opts.source_db.as_deref(), &timer)
            .map(|result| result.reported_at(&opts.path));
    }

    let select_sql = match (&opts.sql, &opts.table) {
        (Some(sql), _) => sql.clone(),
        (None, Some(table)) => {
            // Escape embedded double-quotes per SQL identifier rules to prevent
            // injection via crafted table names from LLM-generated input.
            format!("SELECT * FROM \"{}\"", table.replace('"', "\"\""))
        }
        (None, None) => {
            return Err(McpError::new(
                ErrorCode::SqlError,
                "Either sql or table must be provided",
            ));
        }
    };

    // Every row format splices `select_sql` into `COPY (...) TO <path>`.
    // It must be read-only, and Hyper must parse it on its own first, so
    // it cannot close the parenthesis and name a `TO` target of its own.
    require_read_only_sql("export", &select_sql)?;
    engine.check_embeddable_query(&select_sql)?;

    let extra = opts.format_options.as_ref();
    let result = match opts.format.as_str() {
        "csv" => export_csv(engine, &select_sql, path, extra, &timer),
        "parquet" => export_parquet(engine, &select_sql, path, extra, &timer),
        "arrow_ipc" => export_arrow_ipc(engine, &select_sql, path, extra, &timer),
        "iceberg" => export_iceberg(engine, &select_sql, path, extra, &timer),
        other => Err(McpError::new(
            ErrorCode::UnsupportedFormat,
            format!("Unsupported export format: {other}"),
        )),
    };
    result.map(|result| result.reported_at(&opts.path))
}

/// Validate that an option key like `codec` is a safe identifier.
///
/// We pass the whole `WITH (...)` clause into hyperd as SQL, so an
/// unchecked key like `foo) --` would let a caller rewrite the
/// statement. Allow only ASCII letters, digits, and underscores,
/// starting with a letter or underscore — hyperd's own option names all
/// fit this shape.
fn validate_option_key(key: &str) -> Result<(), McpError> {
    let bad = key.is_empty()
        || !key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if bad {
        return Err(McpError::new(
            ErrorCode::SchemaMismatch,
            format!(
                "format_options key '{key}' must match [A-Za-z_][A-Za-z0-9_]* \
                 (hyperd COPY option names use that shape)"
            ),
        ));
    }
    Ok(())
}

/// Render a single `format_options` value into a SQL literal. Strings
/// are single-quote-escaped; booleans become `true`/`false`; numbers
/// (including fractional) are rendered via `Value::to_string`. Null,
/// nested arrays, and nested objects are rejected with a clear error —
/// hyperd's COPY options are all simple scalars.
fn render_option_value(key: &str, value: &Value) -> Result<String, McpError> {
    match value {
        Value::String(s) => Ok(escape_string_literal(s)),
        Value::Bool(b) => Ok(if *b { "true".into() } else { "false".into() }),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => Err(McpError::new(
            ErrorCode::SchemaMismatch,
            format!(
                "format_options['{key}'] must be a string, boolean, or number \
                 (got {value:?})"
            ),
        )),
    }
}

/// Merge a format-specific base `WITH (...)` clause (e.g.
/// `"format => 'parquet'"`) with caller-supplied `format_options`.
/// Caller options always appear after the base, so if the same key
/// appears in both the caller's value wins.
fn render_copy_with_clause(
    base: &str,
    extra: Option<&Map<String, Value>>,
) -> Result<String, McpError> {
    let mut clause = base.to_string();
    if let Some(opts) = extra {
        for (key, value) in opts {
            validate_option_key(key)?;
            let rendered_value = render_option_value(key, value)?;
            clause.push_str(", ");
            clause.push_str(key);
            clause.push_str(" => ");
            clause.push_str(&rendered_value);
        }
    }
    Ok(clause)
}

/// `COPY (<sql>) TO <path> WITH (<options>)`, with `sql` on lines of its
/// own so a trailing `--` comment in it ends before the `)`. `sql` must
/// have passed [`Engine::check_embeddable_query`].
fn copy_to_sql(sql: &str, quoted_path: &str, with_clause: &str) -> String {
    format!("COPY (\n{sql}\n) TO {quoted_path} WITH ({with_clause})")
}

/// Shared helper: issue a single `COPY (query) TO 'path' WITH (...)` to
/// hyperd and return the reported row count + on-disk file size. All
/// row-oriented exports funnel through this — the format-specific logic
/// is just the `WITH (...)` clause.
///
/// Hyperd handles path I/O itself, so no in-memory buffering and no
/// Rust-side type mapping is involved. Unlike `CREATE TABLE AS`, `COPY`
/// reports the written row count directly in `affected_rows`, so no
/// follow-up `COUNT(*)` is required.
fn run_copy_to(
    engine: &Engine,
    sql: &str,
    path: &str,
    base_with: &str,
    extra_options: Option<&Map<String, Value>>,
    format_label: &str,
    timer: &StatsTimer,
) -> Result<ExportResult, McpError> {
    let with_clause = render_copy_with_clause(base_with, extra_options)?;
    let quoted_path = escape_string_literal(path);
    let copy_sql = copy_to_sql(sql, &quoted_path, &with_clause);
    let row_count = engine.execute_command(&copy_sql)?;

    let file_size = std::fs::metadata(path).map_or(0, |m| m.len());

    Ok(ExportResult {
        rows: row_count,
        stats: ExportStats {
            operation: "export".into(),
            rows: row_count,
            elapsed_ms: timer.elapsed_ms(),
            file_size_bytes: file_size,
            format: format_label.into(),
            output_path: path.into(),
        },
        schema_fidelity: None,
    })
}

/// Export as CSV via hyperd's native `COPY ... WITH (format => 'csv',
/// header => true)`. Hyperd writes the file directly — no in-memory
/// buffer in Rust. Caller can override or extend via `format_options`
/// (e.g. `{"header": false, "delimiter": "\t"}`).
fn export_csv(
    engine: &Engine,
    sql: &str,
    path: &str,
    format_options: Option<&Map<String, Value>>,
    timer: &StatsTimer,
) -> Result<ExportResult, McpError> {
    run_copy_to(
        engine,
        sql,
        path,
        "format => 'csv', header => true",
        format_options,
        "csv",
        timer,
    )
}

/// Export as Parquet via hyperd's native
/// `COPY ... WITH (format => 'parquet')`. Types (NUMERIC precision,
/// DATE, TIMESTAMP, non-null flags, ...) are preserved exactly —
/// hyperd writes its own Arrow schema from the query's `RowDescription`,
/// bypassing any JSON round-trip.
/// Caller can override via `format_options` (e.g. `{"codec":
/// "zstd", "rows_per_row_group": 100000}`).
fn export_parquet(
    engine: &Engine,
    sql: &str,
    path: &str,
    format_options: Option<&Map<String, Value>>,
    timer: &StatsTimer,
) -> Result<ExportResult, McpError> {
    run_copy_to(
        engine,
        sql,
        path,
        "format => 'parquet'",
        format_options,
        "parquet",
        timer,
    )
}

/// Export as Arrow IPC Stream format via hyperd's native
/// `COPY ... WITH (format => 'arrowstream')`. This is the same wire
/// shape Hyper speaks on its binary Arrow query protocol, so the
/// produced bytes are consumable by any Arrow IPC Stream reader and
/// round-trip through our `load_file` path (which auto-detects the
/// sub-format).
fn export_arrow_ipc(
    engine: &Engine,
    sql: &str,
    path: &str,
    format_options: Option<&Map<String, Value>>,
    timer: &StatsTimer,
) -> Result<ExportResult, McpError> {
    run_copy_to(
        engine,
        sql,
        path,
        "format => 'arrowstream'",
        format_options,
        "arrow_ipc",
        timer,
    )
}

/// Export query results as an Apache Iceberg table directory using
/// hyperd's native `COPY (query) TO 'dir' WITH (format => 'iceberg')`.
///
/// Hyperd creates the destination directory with a `metadata/` subdir
/// (snapshot JSONs + manifests) and one or more `data/` parquet files.
/// The produced layout round-trips cleanly back through `load_iceberg`.
///
/// Caller semantics:
/// - `path` is a *directory* path (not a single file). hyperd's `COPY TO`
///   refuses to write into an existing non-empty Iceberg location, so an
///   existing `path` (which `export_to_file` lets through only with
///   `overwrite`) must be an Iceberg table or empty, and is moved aside
///   while the export runs: a failed export puts it back.
fn export_iceberg(
    engine: &Engine,
    sql: &str,
    path: &str,
    format_options: Option<&Map<String, Value>>,
    timer: &StatsTimer,
) -> Result<ExportResult, McpError> {
    let dest = Path::new(path);
    let with_clause = render_copy_with_clause("format => 'iceberg'", format_options)?;
    let quoted_path = escape_string_literal(path);
    let copy_sql = copy_to_sql(sql, &quoted_path, &with_clause);

    let previous = if std::fs::symlink_metadata(dest).is_ok() {
        require_iceberg_table_dir(dest)?;
        Some(ReplaceGuard::move_aside(dest)?)
    } else {
        None
    };
    let row_count = match engine.execute_command(&copy_sql) {
        Ok(rows) => rows,
        Err(e) => {
            return Err(match previous {
                Some(previous) => previous.restore(e),
                None => e,
            });
        }
    };
    if let Some(previous) = previous {
        previous.commit();
    }

    // Directory size = sum of all file sizes under `path`. Not strictly
    // required by callers, but useful for telemetry.
    let file_size = walk_dir_size(dest).unwrap_or(0);

    Ok(ExportResult {
        rows: row_count,
        stats: ExportStats {
            operation: "export".into(),
            rows: row_count,
            elapsed_ms: timer.elapsed_ms(),
            file_size_bytes: file_size,
            format: "iceberg".into(),
            output_path: path.into(),
        },
        schema_fidelity: None,
    })
}

/// Sum the byte sizes of every regular file under `dir`. Used for
/// export telemetry on directory-based formats (Iceberg). Silent on I/O
/// errors — telemetry is best-effort.
fn walk_dir_size(dir: &std::path::Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for entry in std::fs::read_dir(&p)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total = total.saturating_add(entry.metadata().map_or(0, |m| m.len()));
            }
        }
    }
    Ok(total)
}

/// Refuse to replace `dest` unless it is an empty directory or one shaped
/// like hyperd's Iceberg export: top-level entries only `data/` and
/// `metadata/`, with a `*.metadata.json` in `metadata/` (measured against
/// hyperd 0.0.26359, with and without rows).
fn require_iceberg_table_dir(dest: &Path) -> Result<(), McpError> {
    if is_iceberg_table_dir(dest).unwrap_or(false) {
        return Ok(());
    }
    Err(McpError::new(
        ErrorCode::InvalidArgument,
        format!(
            "Refusing to replace '{}': it is not an Iceberg table directory",
            dest.display()
        ),
    )
    .with_suggestion("Export to a new directory, or to an existing Iceberg table to replace it."))
}

fn is_iceberg_table_dir(dest: &Path) -> std::io::Result<bool> {
    if !std::fs::symlink_metadata(dest)?.is_dir() {
        return Ok(false);
    }
    let mut has_metadata = false;
    for entry in std::fs::read_dir(dest)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            return Ok(false);
        }
        match entry.file_name().to_str() {
            Some("data") => {}
            Some("metadata") => {
                for file in std::fs::read_dir(entry.path())? {
                    let file = file?;
                    if file.file_type()?.is_file()
                        && file
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.ends_with(".metadata.json"))
                    {
                        has_metadata = true;
                    }
                }
                if !has_metadata {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    // An empty directory has neither entry, and holds nothing to lose.
    Ok(has_metadata || std::fs::read_dir(dest)?.next().is_none())
}

/// A sibling of `dest` named `<name>.hyperdb-mcp-<tag>-<pid>-<n>`, unique
/// within this process.
fn sibling_path(dest: &Path, tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".hyperdb-mcp-{tag}-{}-{n}", std::process::id()));
    dest.with_file_name(name)
}

/// The previous contents of an export destination, moved to a sibling
/// backup while the export writes a new one.
///
/// [`Self::commit`] deletes the backup. [`Self::restore`], or dropping the
/// guard without either, removes whatever the export wrote and moves the
/// backup back. A backup that cannot be restored is left in place and
/// named, never deleted.
struct ReplaceGuard {
    dest: PathBuf,
    backup: Option<PathBuf>,
}

impl ReplaceGuard {
    fn move_aside(dest: &Path) -> Result<Self, McpError> {
        let backup = sibling_path(dest, "old");
        std::fs::rename(dest, &backup).map_err(|e| {
            McpError::new(
                if is_file_in_use(&e) {
                    ErrorCode::ResourceBusy
                } else {
                    ErrorCode::PermissionDenied
                },
                format!("Cannot move existing '{}' aside: {e}", dest.display()),
            )
        })?;
        Ok(Self {
            dest: dest.to_path_buf(),
            backup: Some(backup),
        })
    }

    /// The export succeeded: delete the previous contents.
    fn commit(mut self) {
        if let Some(backup) = self.backup.take()
            && let Err(e) = std::fs::remove_dir_all(&backup)
        {
            tracing::warn!(
                backup = %backup.display(),
                err = %e,
                "failed to delete the replaced export destination",
            );
        }
    }

    /// The export failed with `err`: put the previous contents back and
    /// return `err`, naming the backup if it could not be restored.
    fn restore(mut self, err: McpError) -> McpError {
        let Some(backup) = self.backup.take() else {
            return err;
        };
        match restore_backup(&self.dest, &backup) {
            Ok(()) => err,
            Err(e) => McpError {
                message: format!(
                    "{} (the previous contents could not be restored and are at '{}': {e})",
                    err.message,
                    backup.display()
                ),
                ..err
            },
        }
    }
}

impl Drop for ReplaceGuard {
    fn drop(&mut self) {
        if let Some(backup) = self.backup.take()
            && let Err(e) = restore_backup(&self.dest, &backup)
        {
            tracing::warn!(
                backup = %backup.display(),
                err = %e,
                "failed to restore the previous export destination",
            );
        }
    }
}

/// Remove what is at `dest` now (written by the failed export, since the
/// original was moved away) and move `backup` back. Something another
/// process created at `dest` while the export ran would be removed too;
/// that race is accepted.
fn restore_backup(dest: &Path, backup: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(dest)?,
        Ok(_) => std::fs::remove_file(dest)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::rename(backup, dest)
}

/// True when a filesystem error means "another process is holding this file
/// open" rather than "you lack permission to touch it".
///
/// This is effectively a Windows condition. Windows refuses to unlink a file
/// another process holds without `FILE_SHARE_DELETE`, reporting
/// `ERROR_SHARING_VIOLATION` (32) or `ERROR_LOCK_VIOLATION` (33); Unix
/// unlinks regardless of open handles, so a contended export target never
/// fails there. `ErrorKind::ResourceBusy` is checked on every platform
/// because `EBUSY` can arise for other reasons (e.g. the target is a
/// mountpoint).
///
/// The raw-code comparison is gated on `cfg!(windows)` deliberately: those
/// two numbers are `EPIPE` and `EDOM` as Unix errnos, and while
/// `remove_file` cannot return either, matching on them unguarded would be
/// a latent misclassification.
fn is_file_in_use(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::ResourceBusy
        || (cfg!(windows) && matches!(err.raw_os_error(), Some(32 | 33)))
}

/// Export the workspace tables as a new `.hyper` file. Issues
/// `CREATE DATABASE` + `ATTACH DATABASE` against the target path and
/// populates it with one constraint-preserving copy per user table.
///
/// We can't just `std::fs::copy(workspace, target)` because on Windows
/// hyperd holds the workspace file open with an exclusive lock, and
/// Windows blocks any concurrent open of a locked file (Unix allows it
/// via shared handle semantics). Going through hyperd keeps this
/// cross-platform at the cost of only copying tables — views,
/// sequences, and other catalog objects in the source are not
/// reproduced. That's acceptable for the current callers (LLMs
/// exporting workspace data for Tableau Desktop), but documented here
/// so a future caller that needs full catalog fidelity knows why.
///
/// Within a table the copy is faithful: `NOT NULL`, `DEFAULT`,
/// `ASSUMED PRIMARY KEY`, and `ASSUMED UNIQUE` all survive. Anything that
/// could not be reproduced comes back in
/// [`ExportResult::schema_fidelity`] so the caller can say so out loud
/// instead of handing back a quietly degraded backup.
fn export_hyper(
    engine: &Engine,
    path: &str,
    source_db: Option<&str>,
    timer: &StatsTimer,
) -> Result<ExportResult, McpError> {
    // `export_to_file` lets an existing `path` through only with
    // `overwrite`; it must then be a Hyper database, and it is not touched
    // until the new one is complete. The new database is built at a
    // sibling temp path (`CREATE DATABASE` refuses an existing file) and
    // renamed over `path`, so a failed export leaves `path` as it was.
    let dest = Path::new(path);
    if std::fs::symlink_metadata(dest).is_ok() {
        require_hyper_file(dest)?;
    }
    let temp = sibling_path(dest, "tmp");
    let temp_str = temp
        .to_str()
        .expect("a sibling of a UTF-8 path with an ASCII suffix is UTF-8");

    let result = build_hyper_export(engine, temp_str, source_db, timer).and_then(|report| {
        std::fs::rename(&temp, dest).map_err(|e| {
            // A file another process is holding open is not a permissions
            // problem, and "Check file permissions on the source or target
            // path" — `PermissionDenied`'s guidance — sends the caller
            // somewhere there is nothing to find. `ResourceBusy` carries
            // the guidance that actually resolves it (close the holder, or
            // copy the file). On Unix the rename succeeds regardless of
            // holders. On Windows, replacing a file held without
            // `FILE_SHARE_DELETE` may instead fail with
            // `ERROR_ACCESS_DENIED`, which cannot be told apart from a real
            // permission error and so stays `PermissionDenied`.
            //
            // The rename also replaces a file created at `dest` after
            // `export_to_file` found nothing there; that race is accepted.
            if is_file_in_use(&e) {
                McpError::new(
                    ErrorCode::ResourceBusy,
                    format!(
                        "Cannot replace export target '{path}': the file is open in another process: {e}"
                    ),
                )
            } else {
                McpError::new(
                    ErrorCode::PermissionDenied,
                    format!("Cannot move the export into place at '{path}': {e}"),
                )
            }
        })?;
        Ok(report)
    });
    let report = match result {
        Ok(report) => report,
        Err(e) => {
            if let Err(remove_err) = std::fs::remove_file(&temp)
                && remove_err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    temp = %temp.display(),
                    err = %remove_err,
                    "failed to remove the temporary hyper export",
                );
            }
            return Err(e);
        }
    };
    let rows = report.rows_copied;

    let file_size = std::fs::metadata(path).map_or(0, |m| m.len());

    Ok(ExportResult {
        rows,
        stats: ExportStats {
            operation: "export".into(),
            rows,
            elapsed_ms: timer.elapsed_ms(),
            file_size_bytes: file_size,
            format: "hyper".into(),
            output_path: path.into(),
        },
        schema_fidelity: Some(report),
    })
}

/// Refuse to replace `dest` unless it is a regular file (not a symlink or
/// a directory) that starts with Hyper's file magic.
fn require_hyper_file(dest: &Path) -> Result<(), McpError> {
    let is_hyper = std::fs::symlink_metadata(dest).is_ok_and(|m| m.is_file()) && {
        let mut magic = [0_u8; 5];
        std::fs::File::open(dest)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic))
            .is_ok_and(|()| &magic == b"Hyper")
    };
    if is_hyper {
        return Ok(());
    }
    Err(McpError::new(
        ErrorCode::InvalidArgument,
        format!(
            "Refusing to replace '{}': it is not a Hyper database file",
            dest.display()
        ),
    )
    .with_suggestion("Export to a new path, or to an existing .hyper file to replace it."))
}

/// Create a database at `path`, fill it with the user tables of
/// `source_db`, and detach it.
fn build_hyper_export(
    engine: &Engine,
    path: &str,
    source_db: Option<&str>,
    timer: &StatsTimer,
) -> Result<CopyTableReport, McpError> {
    // Unique alias so we don't collide with a user-issued attach. The
    // `__export_target_` prefix + PID + nanos makes accidental overlap
    // exceedingly unlikely and stays within the 63-char identifier cap.
    let alias = format!(
        "__export_target_{}_{}",
        std::process::id(),
        timer.elapsed_ms(),
    );

    // Route both statements through the same attach-context error
    // mapper `attach.rs` uses so a lock conflict on the export target
    // (another MCP server or hyperd process holds it) surfaces as
    // `RESOURCE_BUSY` with recovery guidance instead of a generic
    // `SqlError` — see `Engine::execute_attach_command`.
    let target_path = std::path::Path::new(path);
    engine.execute_attach_command(
        &format!("CREATE DATABASE {}", escape_sql_path(path)),
        target_path,
    )?;
    engine.execute_attach_command(
        &format!(
            "ATTACH DATABASE {} AS \"{}\"",
            escape_sql_path(path),
            alias.replace('"', "\"\""),
        ),
        target_path,
    )?;

    let result = populate_export_target(engine, source_db, &alias);

    // Always detach, even on failure — the attach was scoped to this
    // call. A failed detach is logged but not surfaced: the caller
    // cares about the copy outcome, not bookkeeping.
    if let Err(e) = engine.execute_command(&format!(
        "DETACH DATABASE \"{}\"",
        alias.replace('"', "\"\""),
    )) {
        tracing::warn!(
            alias = %alias,
            err = %e.message,
            "failed to detach export target after export_hyper",
        );
    }

    result
}

/// Copy every user table from `source_db` (None → primary) into the
/// database attached as `target_alias`. Returns the aggregated copy report.
/// Excludes `pg_catalog` / `information_schema` (and Hyper's own system
/// schemas) so we only touch user data.
///
/// Each table goes through [`Engine::copy_table_preserving_schema`] rather
/// than `CREATE TABLE AS SELECT`. `CTAS` infers the destination schema from
/// the query's result columns, which carry types but no constraints, so it
/// produced backups whose columns were all nullable with no defaults and no
/// keys — data intact, schema quietly relaxed.
fn populate_export_target(
    engine: &Engine,
    source_db: Option<&str>,
    target_alias: &str,
) -> Result<CopyTableReport, McpError> {
    let escaped_alias = target_alias.replace('"', "\"\"");
    let source = source_db.map_or_else(|| engine.primary_db_name(), str::to_string);
    let escaped_source = source.replace('"', "\"\"");

    let schemas = list_user_schemas(engine, &escaped_source)?;
    let mut report = CopyTableReport::default();

    for schema in &schemas {
        let escaped_schema = schema.replace('"', "\"\"");

        // `public` exists by default on a fresh database; everything
        // else has to be created before we can CREATE TABLE into it.
        if schema != "public" {
            engine.execute_command(&format!(
                "CREATE SCHEMA IF NOT EXISTS \"{escaped_alias}\".\"{escaped_schema}\"",
            ))?;
        }

        let tables = list_user_tables(engine, &escaped_source, schema)?;
        for table in &tables {
            if crate::engine::is_internal_table(table) {
                continue;
            }
            let escaped_table = table.replace('"', "\"\"");
            report.merge(engine.copy_table_preserving_schema(
                &format!("\"{escaped_source}\".\"{escaped_schema}\".\"{escaped_table}\""),
                &format!("\"{escaped_alias}\".\"{escaped_schema}\".\"{escaped_table}\""),
            )?);
        }
    }

    Ok(report)
}

fn list_user_schemas(engine: &Engine, escaped_db: &str) -> Result<Vec<String>, McpError> {
    let sql = format!(
        "SELECT nspname FROM \"{escaped_db}\".pg_catalog.pg_namespace \
         WHERE nspname NOT IN ('pg_catalog', 'pg_temp', 'information_schema') \
         AND nspname NOT LIKE 'pg_%'",
    );
    let rows = engine.execute_query_to_json(&sql)?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            r.get("nspname")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect())
}

fn list_user_tables(
    engine: &Engine,
    escaped_db: &str,
    schema: &str,
) -> Result<Vec<String>, McpError> {
    let sql = format!(
        "SELECT tablename FROM \"{escaped_db}\".pg_catalog.pg_tables WHERE schemaname = {}",
        escape_string_literal(schema),
    );
    let rows = engine.execute_query_to_json(&sql)?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            r.get("tablename")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{is_file_in_use, render_copy_with_clause, validate_option_key};
    use serde_json::{Map, Value, json};

    #[test]
    fn render_clause_without_extras_returns_base() {
        let out = render_copy_with_clause("format => 'parquet'", None).unwrap();
        assert_eq!(out, "format => 'parquet'");
    }

    #[test]
    fn render_clause_appends_extras_after_base() {
        let mut m = Map::new();
        m.insert("compression".into(), Value::String("zstd".into()));
        m.insert("rows_per_row_group".into(), json!(100_000));
        let out = render_copy_with_clause("format => 'parquet'", Some(&m)).unwrap();
        // Map iteration is in insertion / BTreeMap order depending on the
        // serde feature, but both of the original keys must appear and the
        // base must come first.
        assert!(out.starts_with("format => 'parquet', "));
        assert!(out.contains("compression => 'zstd'"));
        assert!(out.contains("rows_per_row_group => 100000"));
    }

    #[test]
    fn render_clause_escapes_string_values() {
        let mut m = Map::new();
        m.insert("delimiter".into(), Value::String("it's".into()));
        let out = render_copy_with_clause("format => 'csv'", Some(&m)).unwrap();
        assert!(
            out.contains("delimiter => 'it''s'"),
            "single quote must be doubled; got: {out}"
        );
    }

    #[test]
    fn render_clause_renders_booleans_and_numbers_raw() {
        let mut m = Map::new();
        m.insert("header".into(), Value::Bool(false));
        m.insert("max_file_size".into(), json!(1048576));
        m.insert("ratio".into(), json!(0.25));
        let out = render_copy_with_clause("format => 'csv'", Some(&m)).unwrap();
        assert!(out.contains("header => false"));
        assert!(out.contains("max_file_size => 1048576"));
        assert!(out.contains("ratio => 0.25"));
    }

    #[test]
    fn render_clause_rejects_null_array_object_values() {
        for value in [Value::Null, json!([1, 2]), json!({"nested": 1})] {
            let mut m = Map::new();
            m.insert("whatever".into(), value.clone());
            let err = render_copy_with_clause("format => 'csv'", Some(&m))
                .expect_err("non-scalar values must be rejected");
            assert!(err.message.contains("whatever"));
        }
    }

    #[test]
    fn validate_option_key_accepts_reasonable_names() {
        for k in [
            "compression",
            "rows_per_row_group",
            "header",
            "h",
            "_leading_underscore",
            "table_scheme",
            "MixedCase", // hyperd canonicalizes, but we don't need to reject
        ] {
            validate_option_key(k).unwrap_or_else(|e| panic!("{k} should be valid: {e:?}"));
        }
    }

    #[test]
    fn validate_option_key_rejects_injection_attempts() {
        for bad in [
            "",
            "1starts_with_digit",
            "has-dash",
            "has space",
            "key;DROP",
            "close)--",
            "quote'",
            "unicode\u{00E9}",
        ] {
            assert!(
                validate_option_key(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    /// The Windows sharing-violation branch can't be provoked on Unix, so
    /// pin the classifier directly from raw OS codes. Windows CI observed
    /// `os error 32` from the export pre-delete against a target `hyperd`
    /// held open, which is what this distinguishes from a real
    /// permissions failure.
    #[test]
    fn file_in_use_is_distinguished_from_permission_denied() {
        assert!(
            is_file_in_use(&std::io::Error::from(std::io::ErrorKind::ResourceBusy)),
            "EBUSY means the file is in use on every platform"
        );
        assert!(
            !is_file_in_use(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            "a genuine permissions failure must keep PERMISSION_DENIED"
        );
        assert!(
            !is_file_in_use(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            "a missing file is not a holder conflict"
        );

        // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33) are
        // Win32 codes, and 32/33 are EPIPE/EDOM as Unix errnos — so this
        // must classify on Windows only.
        for raw in [32, 33] {
            assert_eq!(
                is_file_in_use(&std::io::Error::from_raw_os_error(raw)),
                cfg!(windows),
                "raw OS error {raw} must be a holder conflict on Windows only"
            );
        }
    }
}
