// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! MCP server implementation and tool parameter types.
//!
//! The [`HyperMcpServer`] is the top-level struct registered with the `rmcp`
//! framework. It lazily initializes the [`Engine`] on first tool call and
//! routes each MCP tool invocation to the appropriate ingest / query / export
//! function.
//!
//! Parameter structs derive `JsonSchema` so the MCP `tools/list` response
//! includes full JSON Schema descriptions for each tool's inputs.

use crate::attach::{self, AttachRegistry, AttachRequest, AttachSource, LOCAL_ALIAS};
use crate::chart::{render_chart, ChartFormat, ChartOptions, ChartType};
use crate::engine::{is_read_only_sql, Engine};
use crate::error::{ErrorCode, McpError};
use crate::export::{export_to_file, ExportOptions};
use crate::ingest::{
    detect_file_format, ingest_csv, ingest_csv_file, ingest_csv_file_async, ingest_json,
    ingest_json_file, ingest_json_file_async, InferredFileFormat, IngestOptions,
};
use crate::ingest_arrow::{
    ingest_arrow_ipc_file, ingest_arrow_ipc_file_async, ingest_parquet_file,
    ingest_parquet_file_async,
};
use crate::saved_queries::{build_store, SavedQuery, SavedQueryStore};
use crate::subscriptions::{
    uris_for_table_change, uris_for_workspace_change, SubscriptionRegistry,
};
use base64::Engine as _;
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    AnnotateAble, CallToolResult, Content, GetPromptRequestParams, GetPromptResult, Implementation,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
    PromptMessage, PromptMessageRole, RawResource, RawResourceTemplate, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo, SubscribeRequestParams,
    UnsubscribeRequestParams,
};
use rmcp::service::RequestContext;
use rmcp::{
    prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router, RoleServer,
    ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlformat::{FormatOptions, Indent, QueryParams as SqlQueryParams};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

#[expect(
    unused_imports,
    reason = "imported for use in doc comments that reference the type path"
)]
use rmcp::model::RawTextContent;

/// Number of rows returned by the `hyper://tables/{name}/sample` JSON
/// resource. Kept small so an MCP client can prefetch every table's sample
/// into the LLM context without blowing up the prompt budget.
const TABLE_SAMPLE_ROWS: u64 = 5;

/// Number of rows returned by the `hyper://tables/{name}/csv-sample` CSV
/// resource. Slightly larger than the JSON sample because CSV is a much
/// more compact wire format and the extra rows help LLMs see patterns.
const TABLE_CSV_SAMPLE_ROWS: u64 = 20;

// --- Parameter structs ---
// Field-level doc comments become JSON Schema `description` fields in the
// MCP `tools/list` response, so they are written for the LLM caller.

/// Schema override shape shared by `query_data`, `query_file`, `load_data`,
/// and `load_file`. Documented here once so all four tools can reference it
/// without duplicating the prose in every field doc.
///
/// Pass a JSON object mapping **column name → Hyper type string**, for example:
///
/// ```json
/// { "year": "INT", "population": "BIGINT", "entity": "TEXT" }
/// ```
///
/// Override semantics (applied inside ingest):
/// * Keys are matched to columns **by name** (case-sensitive). Column ordering
///   in the JSON object does not need to match the file; the inferred order
///   from the file is preserved.
/// * Columns *not* listed in the override keep their inferred type — you only
///   need to specify the columns you want to correct.
/// * Types are the Hyper SQL type spellings: `INT`, `BIGINT`, `NUMERIC(38,0)`,
///   `DOUBLE PRECISION`, `TEXT`, `BOOL`, `DATE`, `TIMESTAMP`.
/// * If you get a `SchemaMismatch` with suggestion to widen an integer column,
///   the typical fix is `{ "col": "BIGINT" }` or `{ "col": "NUMERIC(38,0)" }`.
///
/// Before ingesting an unfamiliar file, prefer calling `inspect_file` first —
/// it returns the inferred schema plus per-column min / max / `null_count` so
/// you can build a minimal, correct override in one shot.
///
/// Parameters for the `query_data` one-shot tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryDataParams {
    /// JSON array of objects or CSV text.
    pub data: String,
    /// SQL query to run against the data. Reference the table by
    /// `table_name` (default `data`).
    pub sql: String,
    /// Data format: `"json"` or `"csv"`. Auto-detected from the first byte
    /// when omitted (`[`/`{` → JSON, otherwise CSV).
    pub format: Option<String>,
    /// Table name exposed to the SQL query (default: `data`).
    pub table_name: Option<String>,
    /// Partial schema override keyed by column name: `{"col": "BIGINT", ...}`.
    /// Only the listed columns are overridden; the rest keep their inferred
    /// type. See the struct-level docs on `QueryDataParams` and the
    /// `inspect_file` tool for type choices and diagnostics.
    pub schema: Option<Value>,
}

/// Parameters for the `query_file` one-shot tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryFileParams {
    /// Absolute path to a CSV, Parquet, or Arrow IPC file.
    pub path: String,
    /// SQL query to run. Reference the table by `table_name` (default:
    /// filename stem).
    pub sql: String,
    /// Table name exposed to the SQL query (default: filename stem).
    pub table_name: Option<String>,
    /// Partial schema override keyed by column name: `{"col": "BIGINT", ...}`.
    /// See the docs on `QueryDataParams` for the full spec. Call
    /// `inspect_file` first if you are unsure of the correct types.
    pub schema: Option<Value>,
    /// Optional dot-separated path to extract a nested data array from the
    /// JSON file. Numeric segments index into arrays (e.g., `content.0`).
    /// String values encountered during navigation are automatically parsed
    /// as JSON, handling the common pattern where MCP tool responses contain
    /// stringified JSON payloads.
    pub json_extract_path: Option<String>,
}

/// Parameters for the `load_data` workspace tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadDataParams {
    /// Target table name.
    pub table: String,
    /// JSON array of objects or CSV text.
    pub data: String,
    /// Data format: `"json"` or `"csv"`. Auto-detected when omitted.
    pub format: Option<String>,
    /// `"replace"` (default — drops and recreates the table) or
    /// `"append"` (adds rows to an existing table).
    pub mode: Option<String>,
    /// Partial schema override keyed by column name: `{"col": "BIGINT", ...}`.
    /// See the docs on `QueryDataParams` for the full spec.
    pub schema: Option<Value>,
    /// Target database alias. Omit (or pass `"local"`) to write to the
    /// ephemeral primary. Pass `"persistent"` to write to the durable
    /// database that survives across sessions. Other values target a
    /// user-attached database (must be writable).
    pub database: Option<String>,
    /// Shorthand for `database: "persistent"`. When true, data is written
    /// to the persistent database. If both `database` and `persist` are
    /// set, `database` wins.
    pub persist: Option<bool>,
}

/// Parameters for the `load_file` workspace tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadFileParams {
    /// Target table name.
    pub table: String,
    /// Absolute path to a CSV, Parquet, or Arrow IPC file.
    pub path: String,
    /// `"replace"` (default — drops and recreates the table),
    /// `"append"` (adds rows to an existing table), or `"merge"`
    /// (upserts rows by `merge_key`; new columns in the incoming file
    /// are auto-added via `ALTER TABLE ADD COLUMN`).
    pub mode: Option<String>,
    /// Partial schema override keyed by column name: `{"col": "BIGINT", ...}`.
    /// Only the listed columns are overridden; the rest keep their inferred
    /// type. Call `inspect_file` first if you are unsure — it reports
    /// min / max / `null_count` per column using the exact same inference this
    /// tool uses, so the override you build from its output is guaranteed to
    /// align with the file's actual columns.
    pub schema: Option<Value>,
    /// Optional dot-separated path to extract a nested data array from the
    /// JSON file. Numeric segments index into arrays (e.g., `content.0`).
    /// String values encountered during navigation are automatically parsed
    /// as JSON, handling the common pattern where MCP tool responses contain
    /// stringified JSON payloads.
    pub json_extract_path: Option<String>,
    /// When `mode = "merge"`, the column(s) used to match incoming rows to
    /// existing rows for upsert. Pass a single name (`"job_id"`) or a list
    /// (`["cell", "job_id"]`). Required for merge; rejected with a clear
    /// error if set for `replace` or `append`.
    pub merge_key: Option<MergeKey>,
    /// Target database alias. Omit (or pass `"local"`) to write to the
    /// ephemeral primary. Pass `"persistent"` to write to the durable
    /// database. Other values target a user-attached writable database.
    pub database: Option<String>,
    /// Shorthand for `database: "persistent"`. If both `database` and
    /// `persist` are set, `database` wins.
    pub persist: Option<bool>,
}

/// One or many column names. Accepts either a JSON string `"col"` or
/// a JSON array `["col1", "col2"]` for ergonomics — the tool layer
/// normalizes to `Vec<String>` before passing into the ingest code.
///
/// A custom [`serde::Deserialize`] implementation produces clear
/// errors for wrong shapes (`null`, numbers, objects) instead of
/// the default untagged-enum message ("data did not match any
/// variant of untagged enum MergeKey"), which is opaque from the
/// MCP-tool-call side.
#[derive(Debug, JsonSchema)]
#[schemars(
    title = "MergeKey",
    description = "Either a single column name (string) or a list of column names (array of strings)",
    untagged
)]
pub enum MergeKey {
    Single(String),
    Multi(Vec<String>),
}

impl<'de> Deserialize<'de> for MergeKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(deserializer)?;
        match v {
            serde_json::Value::String(s) => Ok(Self::Single(s)),
            serde_json::Value::Array(arr) => {
                let mut names = Vec::with_capacity(arr.len());
                for (i, item) in arr.into_iter().enumerate() {
                    match item {
                        serde_json::Value::String(s) => names.push(s),
                        other => {
                            return Err(D::Error::custom(format!(
                                "merge_key array element [{i}] must be a string \
                                 (column name); got {other}"
                            )));
                        }
                    }
                }
                Ok(Self::Multi(names))
            }
            other => Err(D::Error::custom(format!(
                "merge_key must be a column name (string) or list of column names \
                 (array of strings); got {other}"
            ))),
        }
    }
}

impl MergeKey {
    /// Materialize as a non-empty `Vec<String>`, or return `None` for the
    /// empty case so callers can convert it into an `InvalidArgument`
    /// error with a context-appropriate message.
    pub fn into_vec(self) -> Option<Vec<String>> {
        let v = match self {
            Self::Single(s) => vec![s],
            Self::Multi(v) => v,
        };
        if v.is_empty() || v.iter().any(String::is_empty) {
            None
        } else {
            Some(v)
        }
    }
}

/// One file entry within a [`LoadFilesParams`] batch. Same shape as
/// [`LoadFileParams`] minus cross-cutting concerns handled at the batch
/// level (the batch-level concurrency knob, etc.).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadFilesEntry {
    /// Target table name.
    pub table: String,
    /// Absolute path to a CSV, Parquet, Arrow IPC, or JSON file.
    pub path: String,
    /// `"replace"` (default), `"append"`, or `"merge"` — see
    /// [`LoadFileParams::mode`] for semantics.
    pub mode: Option<String>,
    /// Partial schema override keyed by column name.
    pub schema: Option<Value>,
    /// Optional JSON extract path — see `LoadFileParams::json_extract_path`.
    pub json_extract_path: Option<String>,
    /// When `mode = "merge"`, the column(s) to match on for upsert. See
    /// [`LoadFileParams::merge_key`].
    pub merge_key: Option<MergeKey>,
}

/// Parameters for the `load_files` workspace tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadFilesParams {
    /// Batch of files to ingest in parallel. Each entry targets its own
    /// table and runs independently — one entry's failure does not abort
    /// the others.
    pub files: Vec<LoadFilesEntry>,
    /// Maximum number of concurrent ingest tasks. Each task checks out
    /// its own connection from a pool sized to match. Default:
    /// `min(files.len(), 8)`. Large parquet ingests are I/O-bound on
    /// hyperd's side; more connections don't help past a certain point
    /// and can starve the primary connection.
    pub concurrency: Option<u32>,
    /// Target database alias. Omit (or pass `"local"`) to write to the
    /// ephemeral primary. Pass `"persistent"` to write to the durable
    /// database. Other values target a user-attached writable database.
    /// Applies to every entry in the batch — multi-target batches are
    /// not supported.
    pub database: Option<String>,
    /// Shorthand for `database: "persistent"`. If both `database` and
    /// `persist` are set, `database` wins.
    pub persist: Option<bool>,
}

/// Validate the (`mode`, `merge_key`) combination at the tool boundary.
/// Returns the normalized `Vec<String>` for merge mode (or `None` for
/// replace/append). Rejects:
///
/// - `mode = "merge"` without `merge_key` → `InvalidArgument`.
/// - `mode = "merge"` with empty / blank-element `merge_key` →
///   `InvalidArgument`.
/// - `mode != "merge"` with `merge_key` set → `InvalidArgument`
///   (catches "I added merge_key but forgot mode" mistakes loudly).
fn validate_merge_args(
    mode: &str,
    merge_key: Option<MergeKey>,
) -> Result<Option<Vec<String>>, McpError> {
    match (mode, merge_key) {
        ("merge", None) => Err(McpError::new(
            ErrorCode::InvalidArgument,
            "mode=merge requires merge_key (a column name or list of column names)",
        )),
        ("merge", Some(mk)) => mk.into_vec().map(Some).ok_or_else(|| {
            McpError::new(
                ErrorCode::InvalidArgument,
                "merge_key must be a non-empty list of non-empty column names",
            )
        }),
        (_, Some(_)) => Err(McpError::new(
            ErrorCode::InvalidArgument,
            "merge_key is only valid with mode=merge",
        )),
        (_, None) => Ok(None),
    }
}

/// Parameters for the `load_iceberg` workspace tool.
///
/// An Iceberg table on disk is a *directory* containing a `metadata/`
/// subdir and one or more `data/` parquet files — hyperd reads the
/// metadata JSON to find the right snapshot and then the data files.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadIcebergParams {
    /// Target Hyper table name.
    pub table: String,
    /// Absolute path to the Iceberg table root (the directory that
    /// contains `metadata/` and `data/`).
    pub path: String,
    /// `"replace"` (default) or `"append"`.
    pub mode: Option<String>,
    /// Optional specific metadata filename to pin a snapshot, e.g.
    /// `"v2.metadata.json"`. If omitted, hyperd uses the latest.
    pub metadata_filename: Option<String>,
    /// Optional snapshot version to read as of.
    pub version_as_of: Option<i64>,
}

/// Parameters for the read-only `query` workspace tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryParams {
    /// SQL SELECT / WITH / EXPLAIN / SHOW / VALUES statement (read-only)
    pub sql: String,
    /// Target database alias for unqualified name resolution. Omit to
    /// query the ephemeral primary. Pass `"persistent"` to route to the
    /// durable database, or any user-attached alias.
    pub database: Option<String>,
}

/// Parameters for the mutating `execute` workspace tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteParams {
    /// DDL/DML SQL statement (CREATE, INSERT, UPDATE, DELETE, DROP, ALTER, COPY, etc.)
    pub sql: String,
    /// Target database alias for unqualified name resolution. Omit to
    /// run against the ephemeral primary. Pass `"persistent"` to write
    /// to the durable database (or a writable user-attached alias).
    pub database: Option<String>,
}

/// Parameters for the `sample` convenience tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SampleParams {
    /// Table name to sample from
    pub table: String,
    /// Number of rows to return (default: 5, max: 100)
    pub n: Option<u64>,
    /// Target database alias. Omit to sample from the ephemeral primary;
    /// pass `"persistent"` or a user-attached alias to sample from there.
    pub database: Option<String>,
}

/// Parameters for the `describe` tool. Both fields are optional to preserve
/// backward compatibility with callers that invoke `describe` with no args
/// to get the full workspace listing.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct DescribeParams {
    /// If set, return the schema and row count for just this table. Omit to
    /// list every public table in the workspace.
    pub table: Option<String>,
    /// Target database alias. Omit to describe tables in the ephemeral
    /// primary; pass `"persistent"` or a user-attached alias to describe
    /// tables in another database.
    pub database: Option<String>,
}

/// Parameters for the `chart` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChartParams {
    /// SQL query returning the data to plot (read-only SELECT/WITH/EXPLAIN/SHOW/VALUES)
    pub sql: String,
    /// Chart type: bar, line, scatter, or histogram
    pub chart_type: String,
    /// X-axis column name (required for bar/line/scatter; histogram uses this as the value column)
    pub x: Option<String>,
    /// Y-axis column name (required for bar/line/scatter)
    pub y: Option<String>,
    /// Optional series/grouping column for colored/grouped multi-series charts
    pub series: Option<String>,
    /// Chart title
    pub title: Option<String>,
    /// Output format: "png" (default) or "svg"
    pub format: Option<String>,
    /// Width in pixels (default 800)
    pub width: Option<u32>,
    /// Height in pixels (default 480)
    pub height: Option<u32>,
    /// Number of bins for histograms (default 20)
    pub bins: Option<u32>,
    /// Treat the x column as categorical rather than numeric. Set to `true`
    /// when plotting a `line` or `scatter` against a `DATE`, `TIMESTAMP`,
    /// enum, or any other non-numeric x column; otherwise the chart will
    /// reject the query with "column is missing or not numeric". Bar
    /// charts are always categorical regardless of this flag.
    pub x_as_category: Option<bool>,
    /// Fix the x-axis range as [min, max]. Omit to auto-scale. Useful when
    /// comparing multiple charts at a consistent scale (e.g. [0, 1500] for
    /// population in millions) or when an outlier would distort auto-scaling.
    /// Ignored for bar charts (which use categorical x positions).
    pub x_range: Option<[f64; 2]>,
    /// Fix the y-axis range as [min, max]. Omit to auto-scale.
    /// Example: [0.0, 1.0] to pin a 0–1 index axis regardless of the data.
    pub y_range: Option<[f64; 2]>,
    /// Map series names to hex colors ("#rrggbb"). Series not listed here
    /// fall back to the default color palette. Example:
    /// {"India": "#e41a1c", "China": "#ff7f0e"}. Only meaningful when a
    /// `series` column is set.
    pub color_map: Option<std::collections::HashMap<String, String>>,
    /// When true, draw the series name as a text label next to each dot
    /// (scatter) or point (line) and suppress the legend box. Best when
    /// each series has exactly one point (e.g. one country per dot).
    /// Defaults to false (legend shown).
    pub label_points: Option<bool>,
    /// Where to write the rendered image. Parent directory is created
    /// automatically. If omitted, a file is auto-generated under the
    /// system temp dir (`<temp>/hyperdb-charts/chart-<ts>-<pid>-<n>.<ext>`).
    /// Combine with `inline=true` to receive the bytes inline AND write
    /// a file; otherwise the file is the sole output.
    pub output_path: Option<String>,
    /// When true, include the PNG/SVG bytes inline in the tool result.
    /// Without `output_path` this also skips the disk write entirely
    /// (pure inline). With `output_path` the file is written *and* the
    /// image is returned inline. Defaults to false — i.e. disk write
    /// only, with a short stats blob that carries the path.
    pub inline: Option<bool>,
    /// When false, refuse to overwrite an existing file at `output_path`
    /// and return `PERMISSION_DENIED` without touching it. Defaults to
    /// true (overwrite silently), matching the `export` tool.
    pub overwrite: Option<bool>,
    /// Target database alias for unqualified name resolution in the
    /// chart's SQL. Omit to query the ephemeral primary. Pass
    /// `"persistent"` or a user-attached alias to chart from there.
    pub database: Option<String>,
}

/// Parameters for the `watch_directory` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WatchDirectoryParams {
    /// Absolute path to the directory to watch
    pub path: String,
    /// Target table name — all files in the directory are appended to this table
    pub table: String,
    /// Maximum number of files ingested in parallel. Defaults to 4; capped at 32.
    /// Each in-flight ingest holds one connection to hyperd plus a transaction.
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    /// Target database alias. Omit (or pass `"local"`) for the ephemeral
    /// primary. Pass `"persistent"` for the durable database, or any
    /// user-attached writable alias. The watcher's connection pool is
    /// built against the resolved target, so subsequent ingests land
    /// in the right database without per-file routing.
    ///
    /// Detaching the alias while a watcher is active is rejected — call
    /// `unwatch_directory` first.
    pub database: Option<String>,
    /// Shorthand for `database: "persistent"`. If both `database` and
    /// `persist` are set, `database` wins.
    pub persist: Option<bool>,
}

/// Parameters for the `unwatch_directory` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnwatchDirectoryParams {
    /// Path of a currently watched directory
    pub path: String,
}

/// Parameters for the `inspect_file` tool.
///
/// Dry-run a file against the same schema inference + numeric-widening pipeline
/// that `load_file` uses, returning the inferred schema plus per-column
/// diagnostics. Call this *before* `load_file` whenever you are unsure about
/// types — especially for wide CSVs with large numbers, mixed integer/float
/// columns, or values that only appear near the end of the file. Use the
/// returned `type` + `min` / `max` to construct an explicit `schema` override
/// for the subsequent `load_file` / `load_data` call.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectFileParams {
    /// Absolute path to the CSV, Parquet, or Arrow IPC file to inspect.
    /// Nothing is written to Hyper and no engine is started.
    pub path: String,
    /// Maximum number of sample rows / values per column to return (default
    /// 5, max 50). Useful for checking that an override would produce the
    /// expected types before ingesting a large file.
    pub sample_rows: Option<u32>,
    /// Optional dot-separated path to extract a nested data array from a
    /// JSON file before inspecting. See `LoadFileParams::json_extract_path`
    /// for the full path syntax and stringified-JSON handling.
    pub json_extract_path: Option<String>,
}

/// Parameters for the `export` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportParams {
    /// SQL query to export (if omitted, exports whole table)
    pub sql: Option<String>,
    /// Table name (used if sql omitted)
    pub table: Option<String>,
    /// Output file path
    pub path: String,
    /// Format: csv, parquet, `arrow_ipc`, iceberg, or hyper. For `iceberg`
    /// the `path` is a *directory* that hyperd will create (the table
    /// root with a `metadata/` and `data/` subdir); for all other
    /// formats it is a single file.
    pub format: String,
    /// If false, refuse to overwrite an existing file at `path` and return
    /// a `PERMISSION_DENIED` error instead. Defaults to true (overwrite
    /// silently) to match pre-flag behavior.
    pub overwrite: Option<bool>,
    /// Optional per-format options passed through into hyperd's `COPY
    /// (query) TO '…' WITH (…)` clause. Keys must match hyperd's own
    /// option names exactly; values must be strings, numbers, or
    /// booleans (null / nested object / array are rejected). Common
    /// knobs:
    ///
    /// * **parquet** — `codec` (`"snappy"` default, `"zstd"`, `"gzip"`,
    ///   `"uncompressed"`, ...), `rows_per_row_group` (int).
    /// * **iceberg** — everything Parquet accepts, plus `table_scheme`
    ///   (`"metastore"` default, `"filesystem"`), `max_file_size`
    ///   (bytes; split data across multiple parquet files).
    /// * **csv** — `header` (bool, default true), `delimiter` (1-char
    ///   string, default `","`), `null` (string printed for NULL,
    ///   default `""`), `quote` (1-char string).
    /// * **`arrow_ipc`** — none commonly needed.
    ///
    /// Ignored for `format = "hyper"` (which isn't a `COPY`).
    pub format_options: Option<Value>,
    /// Source database alias. Omit to read from the ephemeral primary.
    /// Pass `"persistent"` or a user-attached alias to export from there.
    /// In `table` mode, the table name is fully qualified against this
    /// database. In `sql` mode, unqualified names in the SQL resolve
    /// against this database for the duration of the call.
    pub database: Option<String>,
}

/// Parameters for the `save_query` tool.
///
/// Persists a named read-only SQL query. After saving, the query is
/// available as two MCP resources:
///
/// * `hyper://queries/{name}/definition` — JSON metadata (sql, description,
///   `created_at`).
/// * `hyper://queries/{name}/result` — re-runs the SQL on every read and
///   returns the rows + query stats.
///
/// In ephemeral workspaces (no `--workspace`) saved queries live only for
/// the life of the server process; in persistent workspaces they are
/// stored in the `_hyperdb_saved_queries` meta-table and survive restarts.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaveQueryParams {
    /// Unique name identifying the query. Becomes the path component of
    /// the resource URIs — pick something URL-safe and human-readable.
    pub name: String,
    /// The SQL to store. Must be a read-only statement (`SELECT` / `WITH`
    /// / `EXPLAIN` / `SHOW` / `VALUES`); destructive statements are
    /// rejected at save time.
    pub sql: String,
    /// Optional free-form description — what does this query answer?
    pub description: Option<String>,
}

/// Parameters for the `delete_query` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteQueryParams {
    /// Name of the saved query to remove. No-op when the name doesn't
    /// exist; the tool returns `{"deleted": false}` in that case.
    pub name: String,
}

/// One database to attach for the duration of a single `copy_query`
/// call. Same kind-tagged shape as `AttachDatabaseParams` so the
/// vocabulary stays consistent once remote kinds (`tcp` / `grpc`)
/// arrive.
#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub struct AttachSpec {
    /// Alias used to qualify tables from this attachment (e.g. `src`
    /// lets you reference `src.public.customers`). Must be a SQL
    /// identifier and cannot be `local`.
    pub alias: String,
    /// Attachment kind. Only `"local_file"` is supported today; `"tcp"`
    /// (standard remote hyperd) and `"grpc"` (Data 360 read-only Hyper)
    /// are planned.
    pub kind: String,
    /// Absolute path to a `.hyper` file. Required when `kind ==
    /// "local_file"`; ignored otherwise.
    pub path: Option<String>,
    /// If `true`, allow writes into this attachment. Defaults to
    /// `false`. Must also satisfy the server's `--read-only` flag (it
    /// always wins).
    pub writable: Option<bool>,
    /// What to do when `kind == "local_file"` and `path` does not yet
    /// exist. `"error"` (default) returns `FILE_NOT_FOUND`; `"create"`
    /// issues `CREATE DATABASE IF NOT EXISTS` first and then attaches
    /// the resulting empty file. `"create"` requires `writable: true`
    /// and is rejected when the server is `--read-only`.
    pub on_missing: Option<String>,
}

/// Parameters for the `attach_database` tool. Mirrors [`AttachSpec`]
/// except that these attachments live for the rest of the MCP session
/// (or until `detach_database` is called).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AttachDatabaseParams {
    /// Alias to register the attachment under. Must be a SQL identifier
    /// (`[A-Za-z_][A-Za-z0-9_]{0,62}`) and cannot be `local` (reserved
    /// for the primary workspace).
    pub alias: String,
    /// Attachment kind. Only `"local_file"` is supported today.
    pub kind: String,
    /// Absolute path to a `.hyper` file. Required when `kind ==
    /// "local_file"`. The file must be idle — another MCP server or
    /// `hyperd` instance holding it will cause a `RESOURCE_BUSY` error.
    pub path: Option<String>,
    /// If `true`, `copy_query` (and raw `execute`) may target this
    /// attachment. Defaults to `false` so sources stay safe from
    /// accidental mutation.
    pub writable: Option<bool>,
    /// What to do when `kind == "local_file"` and `path` does not yet
    /// exist:
    ///
    /// * `"error"` (default) — return `FILE_NOT_FOUND`. Matches the
    ///   pre-existing contract.
    /// * `"create"` — issue `CREATE DATABASE IF NOT EXISTS` against the
    ///   path first, then attach the resulting empty file. Requires
    ///   `writable: true` (otherwise the empty DB would be unusable)
    ///   and is rejected when the server is running with `--read-only`.
    ///   The parent directory must already exist.
    pub on_missing: Option<String>,
}

/// Parameters for the `detach_database` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DetachDatabaseParams {
    /// Alias of a previously attached database.
    pub alias: String,
}

/// Parameters for the `copy_query` tool. Runs a read-only SELECT / WITH
/// / VALUES statement and lands the result into a target table.
///
/// The inner `sql` may reference tables in the primary workspace
/// (unqualified) as well as tables in any attachment by its fully
/// qualified form — e.g. `src.public.customers`. The destination is
/// resolved via `target_database` (main workspace by default).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CopyQueryParams {
    /// Read-only SQL statement whose result rows will be inserted into
    /// `target_table`. Must begin with `SELECT`, `WITH`, or `VALUES`.
    /// `EXPLAIN` / `SHOW` are rejected because their output shape isn't
    /// row-compatible with a target table.
    pub sql: String,
    /// Unqualified destination table name. Always lands in the
    /// `public` schema of the database identified by `target_database`.
    pub target_table: String,
    /// How to reconcile with any existing target table:
    ///
    /// * `"create"` — error if the target already exists; create from
    ///   the query's result schema via `CREATE TABLE AS`.
    /// * `"append"` — error if the target does not exist; rows are
    ///   appended via `INSERT INTO ... SELECT`.
    /// * `"replace"` — drop (if any) and recreate, atomically.
    pub mode: String,
    /// Alias of the destination database. `None` and `"local"` both
    /// mean the server's primary workspace. Any other value must refer
    /// to an attachment registered with `writable: true`.
    pub target_database: Option<String>,
    /// Optional list of databases to attach for the duration of this
    /// call only. Detached automatically even if the query fails.
    /// Aliases used here must not already be in use.
    pub temp_attach: Option<Vec<AttachSpec>>,
}

/// Parameters for the `set_table_metadata` tool.
///
/// Writes prose fields to the `_table_catalog` row for `table`. Unset
/// fields are left unchanged; passing an explicit empty string (`""`)
/// clears a field. Mechanical fields (`loaded_at`, `last_refreshed_at`,
/// `row_count`, `load_tool`, `load_params`) are managed by the server
/// and cannot be set through this tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetTableMetadataParams {
    /// Target table name. Must already exist in the workspace and have a
    /// catalog entry — load the table first (or run `execute CREATE
    /// TABLE`) so the server auto-stubs the row.
    pub table: String,
    /// Where the data came from (URL, S3 path, internal system name).
    pub source_url: Option<String>,
    /// Short description of the dataset (what's in the table, how to
    /// interpret it).
    pub source_description: Option<String>,
    /// Why this data is in the workspace — what questions it's intended
    /// to answer.
    pub purpose: Option<String>,
    /// License or attribution requirements for the source data.
    pub license: Option<String>,
    /// Free-form notes: refresh instructions, known gotchas, caveats.
    pub notes: Option<String>,
    /// Target database alias for the catalog write. Omit (or pass
    /// `"local"` / `"persistent"`) to update the persistent catalog —
    /// matches the default for the ephemeral primary's tables.
    /// Pass any user-attached writable alias to update that DB's
    /// per-database `_table_catalog` instead. Read-only attachments
    /// are rejected with a clear "re-attach with writable:true"
    /// message.
    pub database: Option<String>,
}

// --- Prompt argument structs ---

/// Arguments for the `analyze-table` prompt.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnalyzeTableArgs {
    /// Name of the table to analyze
    pub table: String,
}

/// Arguments for the `compare-tables` prompt.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompareTablesArgs {
    /// First table to compare
    pub table_a: String,
    /// Second table to compare
    pub table_b: String,
}

/// Arguments for the `data-quality` prompt.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DataQualityArgs {
    /// Name of the table to assess
    pub table: String,
}

/// Arguments for the `suggest-queries` prompt.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SuggestQueriesArgs {
    /// Name of the table to suggest queries for
    pub table: String,
    /// Optional goal or focus area (e.g. "find top customers", "detect anomalies")
    pub goal: Option<String>,
}

// --- Server ---

/// The MCP server that registers all Hyper tools and routes invocations.
///
/// The `Engine` is lazily initialized behind a `Mutex<Option<Engine>>` so that
/// the expensive `HyperProcess` startup only happens on the first actual tool
/// call, not during MCP handshake. This keeps `initialize` fast and avoids
/// starting `hyperd` if the client never calls a tool.
pub struct HyperMcpServer {
    engine: Arc<Mutex<Option<Engine>>>,
    /// `true` once [`Self::ensure_catalog_ready`] has successfully run on
    /// the current engine, so we only try to create / reconcile
    /// `_table_catalog` once per process. Reset to `false` if the
    /// underlying engine is torn down (e.g. connection lost) so the next
    /// call re-bootstraps.
    catalog_ready: Arc<Mutex<bool>>,
    watchers: Arc<crate::watcher::WatcherRegistry>,
    saved_queries: Arc<dyn SavedQueryStore>,
    subscriptions: Arc<SubscriptionRegistry>,
    /// Registry of `ATTACH DATABASE`s requested via `attach_database`.
    /// Lives at the server level (not the engine level) so the list
    /// survives `ConnectionLost` reconnects: [`Self::with_engine`]
    /// calls [`AttachRegistry::replay_all`] after building a fresh
    /// engine.
    attachments: Arc<AttachRegistry>,
    /// Path to the persistent `.hyper` file, or `None` for `--ephemeral-only`.
    /// Threaded into `Engine::new` so the engine can attach it under the
    /// reserved `"persistent"` alias.
    workspace_path: Option<String>,
    read_only: bool,
    /// Skip the shared daemon and spawn a private `hyperd` (legacy behavior).
    no_daemon: bool,
    /// Last time a heartbeat was sent to the daemon (debounced to avoid per-call TCP overhead).
    last_heartbeat: std::sync::Mutex<std::time::Instant>,
    // Under rmcp 1.x the router fields are constructed for downstream
    // macro-generated dispatch but not read through a direct field access
    // that the compiler can see. Keep them; the `#[tool_router]` /
    // `#[prompt_router]` attribute macros on impl blocks wire the routing.
    #[expect(dead_code, reason = "constructed for rmcp 1.x macro-based dispatch")]
    tool_router: ToolRouter<Self>,
    #[expect(dead_code, reason = "constructed for rmcp 1.x macro-based dispatch")]
    prompt_router: PromptRouter<Self>,
}

impl std::fmt::Debug for HyperMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperMcpServer")
            .field("persistent_path", &self.workspace_path)
            .field("read_only", &self.read_only)
            .field("no_daemon", &self.no_daemon)
            .finish_non_exhaustive()
    }
}

impl HyperMcpServer {
    /// Create a server instance. Pass `Some(path)` for persistent workspace,
    /// `None` for ephemeral (temp directory, auto-cleaned).
    ///
    /// The saved-queries store is chosen to match the workspace mode:
    /// persistent workspaces get a [`crate::saved_queries::WorkspaceStore`]
    /// (backed by a meta-table in the `.hyper` file so queries survive
    /// restarts), ephemeral workspaces get an in-memory
    /// [`crate::saved_queries::SessionStore`].
    ///
    /// When `read_only` is `true`, the `execute`, `load_data`, `load_file`,
    /// `save_query`, `delete_query`, and `set_table_metadata` tools return
    /// a `ReadOnlyViolation` error, and exporting to the `hyper` format
    /// (which is a raw file copy, harmless) remains allowed.
    ///
    /// When `bare` is `true`, the server does not create or maintain the
    /// `_table_catalog` table, and saved queries fall back to the in-memory
    /// [`crate::saved_queries::SessionStore`] regardless of `workspace_path`
    /// `persistent_path` is the resolved path to the persistent database
    /// (`Some`) or `None` for `--ephemeral-only` mode.
    pub fn new(persistent_path: Option<String>, read_only: bool) -> Self {
        Self::with_options(persistent_path, read_only, false)
    }

    /// Create a server instance with explicit daemon control.
    pub fn with_no_daemon(
        persistent_path: Option<String>,
        read_only: bool,
        no_daemon: bool,
    ) -> Self {
        Self::with_options(persistent_path, read_only, no_daemon)
    }

    fn with_options(persistent_path: Option<String>, read_only: bool, no_daemon: bool) -> Self {
        // Saved queries persist when a persistent database is available;
        // session storage takes over for `--ephemeral-only` sessions.
        let saved_queries: Arc<dyn SavedQueryStore> = build_store(persistent_path.as_deref());
        Self {
            engine: Arc::new(Mutex::new(None)),
            catalog_ready: Arc::new(Mutex::new(false)),
            watchers: Arc::new(crate::watcher::WatcherRegistry::new()),
            saved_queries,
            subscriptions: Arc::new(SubscriptionRegistry::new()),
            // The catalog policy is now uniform: seed `_table_catalog`
            // whenever MCP creates a fresh `.hyper` file. The opt-out
            // `--bare` path was removed; users wanting a pristine file
            // can `DROP TABLE _table_catalog` after creation.
            attachments: Arc::new(AttachRegistry::new()),
            workspace_path: persistent_path,
            read_only,
            no_daemon,
            last_heartbeat: std::sync::Mutex::new(std::time::Instant::now()),
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    /// Return a clone of the subscription registry so background tasks
    /// (notably the directory watcher) can fire resource updates after
    /// their own ingest completes.
    #[must_use]
    pub fn subscriptions_handle(&self) -> Arc<SubscriptionRegistry> {
        Arc::clone(&self.subscriptions)
    }

    /// Fire resource-updated notifications for every URI affected by a
    /// change to the given table. Targets the workspace/table-list/readme
    /// summary resources plus the three per-table URIs (schema, sample,
    /// csv-sample). Callers that have just added or dropped a table
    /// should also call [`Self::notify_resource_list_changed`] so
    /// subscribers refresh their resource catalog.
    pub(crate) fn notify_table_changed(&self, table: &str) {
        for uri in uris_for_table_change(table) {
            self.subscriptions.notify_updated(&uri);
        }
    }

    /// Fire updates for every URI that summarises the workspace as a
    /// whole (workspace, tables list, readme). Used after watcher-style
    /// bulk mutations where the single-table helper isn't specific
    /// enough.
    pub(crate) fn notify_workspace_changed(&self) {
        for uri in uris_for_workspace_change() {
            self.subscriptions.notify_updated(uri);
        }
    }

    /// Fire a `notifications/resources/list_changed` broadcast. Call
    /// after any operation that adds or removes resources from the
    /// `resources/list` catalog — dropped tables, saved queries
    /// created / deleted, watcher ingest of a brand-new table.
    pub(crate) fn notify_resource_list_changed(&self) {
        self.subscriptions.notify_list_changed();
    }

    /// Return a clone of the engine Arc so background tasks (watchers) can
    /// share access to the same lazy-initialized engine instance.
    #[must_use]
    pub fn engine_handle(&self) -> Arc<Mutex<Option<Engine>>> {
        Arc::clone(&self.engine)
    }

    /// Return a clone of the watcher registry handle for tool handlers.
    #[must_use]
    pub fn watchers_handle(&self) -> Arc<crate::watcher::WatcherRegistry> {
        Arc::clone(&self.watchers)
    }

    /// Return a clone of the attachments registry handle for tool
    /// handlers and the `with_engine` replay path.
    #[must_use]
    pub fn attachments_handle(&self) -> Arc<AttachRegistry> {
        Arc::clone(&self.attachments)
    }

    /// Whether the server is running in read-only mode.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Return a `ReadOnlyViolation` error if the server is in read-only mode.
    /// Used as an early guard at the top of mutating tool handlers.
    fn check_writable(&self, operation: &str) -> Result<(), McpError> {
        if self.read_only {
            Err(McpError::new(
                ErrorCode::ReadOnlyViolation,
                format!("Operation '{operation}' is not permitted in read-only mode"),
            ))
        } else {
            Ok(())
        }
    }

    /// Resolve the effective database alias from a tool's `database` and
    /// `persist` parameters. Returns `None` when the target is the primary
    /// (ephemeral) — callers should leave SQL unqualified. Returns
    /// `Some(alias)` when targeting a non-primary database.
    ///
    /// When `require_writable` is true, verifies the target alias is
    /// either the primary, `"persistent"` (always writable), or a
    /// user-attached database with `writable: true`.
    fn resolve_db(
        &self,
        engine: &Engine,
        database: Option<&str>,
        persist: Option<bool>,
        require_writable: bool,
    ) -> Result<Option<String>, McpError> {
        let effective = match (database, persist) {
            (Some(db), _) => Some(db),
            (None, Some(true)) => Some(Engine::PERSISTENT_ALIAS),
            _ => None,
        };
        // Filter LOCAL_ALIAS ("local") — treat as primary
        let effective = effective.filter(|s| !s.eq_ignore_ascii_case(crate::attach::LOCAL_ALIAS));

        let resolved = engine.resolve_target_db(effective)?;
        let primary = engine.primary_db_name();

        if resolved == primary {
            return Ok(None);
        }

        if require_writable && resolved != Engine::PERSISTENT_ALIAS {
            match self.attachments.get(&resolved) {
                None => {
                    return Err(McpError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "database '{resolved}' is not attached. \
                             Call attach_database first, or use \"persistent\"."
                        ),
                    ));
                }
                Some(entry) if !entry.writable => {
                    return Err(McpError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "database '{resolved}' was attached read-only. \
                             Re-attach with writable:true to write to it."
                        ),
                    ));
                }
                _ => {}
            }
        }

        Ok(Some(resolved))
    }

    /// Lazily start the Hyper engine on first use, returning a mutex guard
    /// that holds a reference to the initialized `Engine`.
    ///
    /// When the engine was just created, resets the
    /// [`Self::catalog_ready`] flag so the subsequent `with_engine` call
    /// runs the catalog bootstrap. We can't run the bootstrap here
    /// because it needs to issue SQL back through `Engine`, and we're
    /// still holding the outer lock.
    fn ensure_engine(&self) -> Result<std::sync::MutexGuard<'_, Option<Engine>>, McpError> {
        let mut guard = self
            .engine
            .lock()
            .map_err(|_| McpError::new(ErrorCode::InternalError, "Lock poisoned"))?;
        if guard.is_none() {
            tracing::info!(
                persistent_db = self.workspace_path.as_deref().unwrap_or("<ephemeral-only>"),
                no_daemon = self.no_daemon,
                "initializing hyper engine"
            );
            let engine = if self.no_daemon {
                Engine::new_no_daemon(self.workspace_path.clone())?
            } else {
                Engine::new(self.workspace_path.clone())?
            };
            tracing::info!(
                ephemeral_path = %engine.ephemeral_path().display(),
                persistent_path = ?engine.persistent_path(),
                log_dir = %engine.log_dir().display(),
                "engine ready"
            );
            // Replay any attachments tracked across the previous
            // engine's lifetime *before* handing the engine out to a
            // tool — otherwise the first post-reconnect tool call
            // would see the attachments missing from Hyper's view even
            // though the registry still lists them. Logs replay
            // failures; those entries are dropped from the registry
            // inside `replay_all` so a single stale attachment doesn't
            // block recovery.
            if let Err(e) = self.attachments.replay_all(&engine) {
                tracing::warn!(err = %e.message, "failed to replay attachments on new engine");
            }
            *guard = Some(engine);
            // New engine → catalog may need to be created/reconciled
            // even if we already did it against a prior (now-dead)
            // engine.
            if let Ok(mut ready) = self.catalog_ready.lock() {
                *ready = false;
            }
        }
        Ok(guard)
    }

    /// Idempotently create and reconcile `_table_catalog` on first call
    /// per engine. No-op in bare or read-only mode (read-only can't
    /// mutate; bare callers never wanted the catalog in the first place).
    ///
    /// Catalog failures during bootstrap are logged at WARN but do not
    /// fail the outer tool call — a broken catalog should never block a
    /// legitimate query. The `catalog_ready` flag still flips to `true`
    /// so we don't retry the same failing bootstrap on every call.
    fn ensure_catalog_ready(&self, engine: &Engine) {
        if self.read_only {
            return;
        }
        let Ok(mut ready) = self.catalog_ready.lock() else {
            return;
        };
        if *ready {
            return;
        }
        if let Err(e) = crate::table_catalog::ensure_exists(engine) {
            tracing::warn!(err = %e.message, "failed to ensure _table_catalog exists");
        }
        if let Err(e) = crate::table_catalog::reconcile(engine) {
            tracing::warn!(err = %e.message, "failed to reconcile _table_catalog on startup");
        }
        *ready = true;
    }

    /// Best-effort catalog upsert after a successful ingest. Logs and
    /// swallows errors — a bookkeeping failure should never fail an
    /// otherwise-successful load.
    ///
    /// Routes the upsert to `target_db`'s `_table_catalog`. The
    /// catalog is lazily seeded if absent. `target_db = None` and
    /// `target_db = Some("persistent")` both write to the persistent
    /// catalog (the single-engine ephemeral primary stubs survive
    /// there for the session). User-attached writable aliases get
    /// their own per-DB catalog. Read-only attachments are rejected
    /// upstream by `resolve_db(require_writable=true)` so this helper
    /// never sees them.
    #[expect(
        clippy::unused_self,
        reason = "&self required for method-call dispatch; body uses only engine + params"
    )]
    fn after_ingest_catalog_update(
        &self,
        engine: &Engine,
        table_name: &str,
        load_tool: &'static str,
        load_params: Option<&str>,
        row_count: Option<i64>,
        target_db: Option<&str>,
    ) {
        if let Err(e) = crate::table_catalog::upsert_stub_in(
            engine,
            table_name,
            load_tool,
            load_params,
            row_count,
            true,
            target_db,
        ) {
            tracing::warn!(
                table = %table_name,
                target_db = ?target_db,
                err = %e.message,
                "failed to update _table_catalog after ingest"
            );
        }
    }

    /// Best-effort catalog reconcile after a DDL/DML `execute`. Same
    /// error-swallowing rationale as [`Self::after_ingest_catalog_update`].
    ///
    /// Reconciles persistent first, then the user-attached writable
    /// target if one was passed and it isn't persistent. Without the
    /// second pass, raw DDL like `DROP TABLE` against a user-attached
    /// alias leaves the dropped table's row stranded in that DB's
    /// `_table_catalog` indefinitely (bootstrap reconcile only walks
    /// persistent, and tools like `describe` would keep listing it).
    #[expect(
        clippy::unused_self,
        reason = "&self required for method-call dispatch; body uses only engine + target_db"
    )]
    fn after_execute_catalog_update(&self, engine: &Engine, target_db: Option<&str>) {
        if let Err(e) = crate::table_catalog::reconcile_in(engine, None) {
            tracing::warn!(
                err = %e.message,
                "failed to reconcile persistent _table_catalog after execute"
            );
        }
        if let Some(alias) = target_db {
            if !alias.eq_ignore_ascii_case(Engine::PERSISTENT_ALIAS) {
                if let Err(e) = crate::table_catalog::reconcile_in(engine, Some(alias)) {
                    tracing::warn!(
                        target_db = alias,
                        err = %e.message,
                        "failed to reconcile user-DB _table_catalog after execute"
                    );
                }
            }
        }
    }

    /// Convenience wrapper: acquire the engine and run a closure against it.
    ///
    /// If the closure returns an error classified as
    /// [`ErrorCode::ConnectionLost`], the engine is dropped from the mutex
    /// before the error is returned to the caller. The next tool call will
    /// observe `engine.is_none()` and transparently re-spawn `hyperd` via
    /// [`Self::ensure_engine`]. Callers then just retry and the server
    /// heals itself.
    fn with_engine<F, R>(&self, f: F) -> Result<R, McpError>
    where
        F: FnOnce(&Engine) -> Result<R, McpError>,
    {
        let mut guard = self.ensure_engine()?;
        let engine = guard.as_ref().expect("ensure_engine guarantees Some");
        // Bootstrap the catalog exactly once per engine. Intentionally
        // runs *inside* `with_engine` (not `ensure_engine`) so the
        // catalog SQL can see errors classified via the normal error
        // path. No-op in bare or read-only mode.
        self.ensure_catalog_ready(engine);
        // In daemon mode, send a heartbeat so the daemon knows we're still active.
        // Debounced to avoid per-call TCP overhead (only sends if >60s since last).
        if !self.no_daemon {
            self.maybe_send_heartbeat();
        }
        let result = f(engine);
        if let Err(e) = &result {
            tracing::debug!(code = ?e.code, message = %e.message, "tool call returned error");
            if e.code == ErrorCode::ConnectionLost {
                tracing::warn!(
                    // Matches both the "hyperd crashed / socket closed" family
                    // and the "wire desynchronized" family — see
                    // [`crate::error::is_connection_lost`] for the full
                    // classifier and both triggers.
                    "connection to hyperd lost or desynchronized ({}); \
                     dropping engine so next call reconnects",
                    e.message
                );
                *guard = None;
                // Reset so the next call re-bootstraps the catalog
                // against the fresh engine.
                if let Ok(mut ready) = self.catalog_ready.lock() {
                    *ready = false;
                }
                // Tell the daemon hyperd looks dead from over here. The daemon
                // will pick up the flag on its next monitor tick and restart.
                // Skipped in --no-daemon mode because there's no daemon to tell.
                if !self.no_daemon {
                    crate::daemon::health::report_hyperd_error_to_daemon();
                }
            }
        }
        result
    }

    /// Best-effort heartbeat to keep the daemon alive while this client is active.
    /// Debounced: only sends if more than 60 seconds have elapsed since the last heartbeat,
    /// avoiding a new TCP connection on every tool call.
    fn maybe_send_heartbeat(&self) {
        const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
        let should_send = self
            .last_heartbeat
            .lock()
            .is_ok_and(|guard| guard.elapsed() >= HEARTBEAT_INTERVAL);
        if should_send {
            let port = crate::daemon::discovery::resolve_port();
            let _ = crate::daemon::health::send_command(port, "HEARTBEAT");
            if let Ok(mut guard) = self.last_heartbeat.lock() {
                *guard = std::time::Instant::now();
            }
        }
    }

    /// Run a closure that accesses the saved-query store.
    ///
    /// Some store variants (notably
    /// [`crate::saved_queries::WorkspaceStore`]) need an `Engine` handle
    /// to run SQL against the meta-table; others
    /// ([`crate::saved_queries::SessionStore`]) ignore the engine entirely.
    /// For persistent workspaces we spin the engine up lazily (same path
    /// as every tool call), for ephemeral workspaces we skip it so the
    /// session-only store doesn't pay a `hyperd` startup tax.
    fn with_saved_query_store<F, R>(&self, f: F) -> Result<R, McpError>
    where
        F: FnOnce(Option<&Engine>) -> Result<R, McpError>,
    {
        if self.workspace_path.is_some() {
            self.with_engine(|engine| f(Some(engine)))
        } else {
            f(None)
        }
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "signature retained for API symmetry / future fallibility; returning Result/Option keeps callers from breaking when the function later grows failure cases"
    )]
    /// Wrap a successful JSON value as an MCP `CallToolResult` with both
    /// `structuredContent` (for spec-2025-06-18 typed clients) and a
    /// pretty-printed `text` block (for older clients that don't yet read
    /// `structuredContent`). Both representations carry the same JSON.
    fn ok_content(val: Value) -> Result<CallToolResult, rmcp::ErrorData> {
        let text = serde_json::to_string_pretty(&val).unwrap_or_default();
        let mut result = CallToolResult::structured(val);
        // CallToolResult::structured includes a stringified copy in `content`;
        // replace it with a pretty-printed version for human-readable display
        // in older clients.
        result.content = vec![Content::text(text)];
        Ok(result)
    }

    /// Pretty-print a SQL string using the `PostgreSQL` dialect formatter.
    /// Falls back to the original string if formatting fails or produces empty output.
    fn fmt_sql(sql: &str) -> String {
        let opts = FormatOptions {
            indent: Indent::Spaces(2),
            uppercase: Some(true),
            lines_between_queries: 1,
            ..FormatOptions::default()
        };
        let formatted = sqlformat::format(sql, &SqlQueryParams::None, &opts);
        if formatted.trim().is_empty() {
            sql.to_owned()
        } else {
            formatted
        }
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "signature retained for API symmetry / future fallibility; returning Result/Option keeps callers from breaking when the function later grows failure cases"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "call-site ergonomics: function consumes logically-owned parameters, refactoring signatures is not worth per-site churn"
    )]
    /// Wrap an `McpError` as an MCP `CallToolResult` with `isError: true`.
    /// The structured error (code + message + suggestion) is exposed both as
    /// `structuredContent` (spec 2025-06-18) and as a pretty-printed text block
    /// for older clients.
    fn err_content(e: McpError) -> Result<CallToolResult, rmcp::ErrorData> {
        let err_val = serde_json::to_value(&e).unwrap_or(Value::String(e.to_string()));
        let body = json!({"error": err_val});
        let text = serde_json::to_string_pretty(&body).unwrap_or_default();
        let mut result = CallToolResult::structured_error(body);
        result.content = vec![Content::text(text)];
        Ok(result)
    }
}

#[tool_router]
impl HyperMcpServer {
    /// Ingest inline data (JSON or CSV) and run a SQL query in one call. Creates a temp table, queries, discards.
    #[tool(
        description = "Ingest inline data (JSON or CSV) and run a SQL query in one call. Creates a temp table, queries, discards."
    )]
    fn query_data(
        &self,
        Parameters(params): Parameters<QueryDataParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(|engine| {
            let tname = params.table_name.unwrap_or_else(|| "data".into());
            let temp_table = format!("_tmp_{}_{}", tname, rand_suffix());
            let fmt = params.format.unwrap_or_else(|| detect_format(&params.data));
            let schema_override = crate::schema::normalize_schema_param(params.schema.as_ref())?;
            let opts = IngestOptions {
                table: temp_table.clone(),
                mode: "replace".into(),
                schema_override,
                merge_key: None,
                target_db: None,
            };

            let ingest_result = match fmt.as_str() {
                "csv" => ingest_csv(engine, &params.data, &opts),
                _ => ingest_json(engine, &params.data, &opts),
            }?;

            let query_sql = params.sql.replace(&tname, &temp_table);
            let rows = engine.execute_query_to_json(&query_sql)?;
            let _ = engine.execute_command(&format!("DROP TABLE IF EXISTS \"{temp_table}\""));

            Ok(json!({
                "sql": Self::fmt_sql(&params.sql),
                "result": rows,
                "stats": ingest_result.stats.to_json(),
            }))
        });

        match result {
            Ok(val) => Self::ok_content(val),
            Err(e) => Self::err_content(e),
        }
    }

    /// Ingest a file (CSV, JSON, JSONL, Parquet, Arrow IPC) and run a SQL query in one call.
    #[tool(
        description = "Ingest a file (CSV, JSON, JSONL / NDJSON, Parquet, Arrow IPC) and run a SQL query in one call. JSON files may be either a top-level array of objects or newline-delimited JSON (JSONL); the format is auto-detected from the first byte. Use `json_extract_path` to extract a nested data array from a JSON wrapper file (e.g., MCP tool responses saved to disk). The path is dot-separated; numeric segments index into arrays; string values are automatically parsed as JSON."
    )]
    fn query_file(
        &self,
        Parameters(params): Parameters<QueryFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(|engine| {
            crate::attach::validate_input_path(&params.path, "data file")?;
            let stem = std::path::Path::new(&params.path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("file")
                .to_string();
            let tname = params.table_name.unwrap_or_else(|| stem.clone());
            let temp_table = format!("_tmp_{}_{}", tname, rand_suffix());
            let schema_override = crate::schema::normalize_schema_param(params.schema.as_ref())?;
            let opts = IngestOptions {
                table: temp_table.clone(),
                mode: "replace".into(),
                schema_override,
                merge_key: None,
                target_db: None,
            };

            let ingest_result = if let Some(ref json_path) = params.json_extract_path {
                let raw = std::fs::read_to_string(&params.path).map_err(|e| {
                    McpError::new(
                        ErrorCode::FileNotFound,
                        format!("Cannot read file '{}': {e}", params.path),
                    )
                })?;
                let extracted = crate::ingest::extract_json_path(&raw, json_path)?;
                let array_text = crate::ingest::normalize_json_or_jsonl(&extracted)?;
                let mut result = ingest_json(engine, &array_text, &opts)?;
                result.stats.operation = "query_file".into();
                result.stats.bytes_read = std::fs::metadata(&params.path).map_or(0, |m| m.len());
                result.stats.file_format = Some("json".into());
                result
            } else {
                match detect_file_format(std::path::Path::new(&params.path)) {
                    InferredFileFormat::Parquet => ingest_parquet_file(engine, &params.path, &opts),
                    InferredFileFormat::ArrowIpc => {
                        ingest_arrow_ipc_file(engine, &params.path, &opts)
                    }
                    InferredFileFormat::Json => ingest_json_file(engine, &params.path, &opts),
                    InferredFileFormat::Csv => ingest_csv_file(engine, &params.path, &opts),
                }?
            };

            let query_sql = params.sql.replace(&tname, &temp_table);
            let rows = engine.execute_query_to_json(&query_sql)?;
            let _ = engine.execute_command(&format!("DROP TABLE IF EXISTS \"{temp_table}\""));

            Ok(json!({
                "sql": Self::fmt_sql(&params.sql),
                "result": rows,
                "stats": ingest_result.stats.to_json(),
            }))
        });

        match result {
            Ok(val) => Self::ok_content(val),
            Err(e) => Self::err_content(e),
        }
    }

    /// Load inline data (JSON or CSV) into a named workspace table.
    #[tool(
        description = "Load inline data (JSON or CSV) into a named workspace table. Supports partial `schema` overrides keyed by column name — only list the columns you want to correct, the rest keep their inferred type. On SchemaMismatch / numeric overflow, follow the error's suggestion (typically widen an INT column to BIGINT or NUMERIC(38,0))."
    )]
    fn load_data(
        &self,
        Parameters(params): Parameters<LoadDataParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("load_data") {
            return Self::err_content(e);
        }
        let table_name = params.table.clone();
        // Replace-mode creates the table from scratch (or replaces an
        // existing one), which is a resource-list-changing event; append
        // mode only changes row content. Captured before the move-into
        // closure so we can pick the right notifications after success.
        let mode = params.mode.clone().unwrap_or_else(|| "replace".into());
        let result = self.with_engine(|engine| {
            let target_db =
                self.resolve_db(engine, params.database.as_deref(), params.persist, true)?;
            let fmt = params.format.unwrap_or_else(|| detect_format(&params.data));
            let schema_override = crate::schema::normalize_schema_param(params.schema.as_ref())?;
            let opts = IngestOptions {
                table: params.table.clone(),
                mode: mode.clone(),
                schema_override,
                merge_key: None,
                target_db: target_db.clone(),
            };

            let ingest_result = match fmt.as_str() {
                "csv" => ingest_csv(engine, &params.data, &opts),
                _ => ingest_json(engine, &params.data, &opts),
            }?;

            let schema_json: Vec<Value> = ingest_result
                .schema
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name,
                        "type": c.hyper_type,
                        "nullable": c.nullable,
                    })
                })
                .collect();

            // Catalog bookkeeping: the helper routes the upsert to
            // target_db's per-DB _table_catalog (lazily seeded on
            // first ingest). Persistent and ephemeral primary share
            // persistent's catalog; user-attached writable DBs each
            // get their own.
            {
                let load_params = serde_json::to_string(&json!({
                    "mode": mode,
                    "format": fmt,
                    "database": target_db.as_deref().unwrap_or("local"),
                }))
                .ok();
                self.after_ingest_catalog_update(
                    engine,
                    &params.table,
                    "load_data",
                    load_params.as_deref(),
                    i64::try_from(ingest_result.rows).ok(),
                    target_db.as_deref(),
                );
            }

            Ok(json!({
                "rows": ingest_result.rows,
                "schema": schema_json,
                "stats": ingest_result.stats.to_json(),
            }))
        });

        match result {
            Ok(val) => {
                self.notify_table_changed(&table_name);
                if mode == "replace" {
                    // Replace either created a new table or recreated an
                    // existing one — either way the resource catalog
                    // moved.
                    self.notify_resource_list_changed();
                }
                Self::ok_content(val)
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Load a file (CSV, JSON, JSONL, Parquet, Arrow IPC) into a named workspace table.
    #[tool(
        description = "Load a CSV / JSON / JSONL / NDJSON / Parquet / Arrow IPC file into a named workspace table. Format is auto-detected from extension (or content for JSON vs CSV).\n\nWhen choosing a format for *new* data going into Hyper, prefer in this order:\n  1. **Parquet** (fastest, server-side): hyperd reads the file directly via `external()`. Types, NUMERIC precision, DATE / TIMESTAMP, and Snappy/ZSTD compression all preserved. This is the recommended format for large imports.\n  2. **CSV**: server-side `COPY FROM` — also fast, but types are inferred from a header + full-file numeric widening pass (CSV has no embedded type info), and empty unquoted cells load as SQL NULL per PostgreSQL CSV default.\n  3. **Arrow IPC** (.arrow / .ipc / .feather, File or Stream format, auto-detected): read in Rust and streamed into hyperd via the binary COPY protocol with zero value-level decoding. Fast but not quite as fast as Parquet, and schema overrides are rejected (the Arrow schema is authoritative).\n  4. **JSON / JSONL / NDJSON**: parsed in Rust (hyperd has no native JSON reader), with per-row insertion. Use for small / irregular data; large JSON should be converted to Parquet first.\n\nFor Apache Iceberg tables use `load_iceberg` instead — it takes a directory path rather than a single file.\n\nSupports partial `schema` overrides keyed by column name (`{\"col\":\"BIGINT\"}`) — only list columns you want to correct; unlisted columns keep their inferred type. Overrides are supported for Parquet, CSV, and JSON; rejected for Arrow IPC. Call `inspect_file` first when unsure about types or to debug a prior failure; the inspector reports per-column min/max/null_count using the exact same inference logic. Use `json_extract_path` to extract a nested data array from a JSON wrapper file — dot-separated path, numeric segments index into arrays, string values are parsed as JSON.\n\n**Mode**: `replace` (default — drops + recreates the table), `append` (adds rows to an existing table), or `merge` (upserts rows by `merge_key`). In merge mode, set `merge_key` to a column name (`\"job_id\"`) or list of names (`[\"cell\",\"job_id\"]`); rows with a matching key are replaced, rows with no match are inserted. New columns in the incoming file are auto-added via `ALTER TABLE ADD COLUMN`. Type changes on existing columns are rejected — use `replace` for breaking schema changes."
    )]
    fn load_file(
        &self,
        Parameters(params): Parameters<LoadFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("load_file") {
            return Self::err_content(e);
        }
        let table_name = params.table.clone();
        let mode = params.mode.clone().unwrap_or_else(|| "replace".into());
        // Validate (mode, merge_key) combination at the tool boundary so the
        // ingest layer can stay focused on the load mechanics. `merge_key`
        // is required for merge and rejected for replace/append.
        let merge_key_vec = match validate_merge_args(&mode, params.merge_key) {
            Ok(v) => v,
            Err(e) => return Self::err_content(e),
        };
        // The closure returns `(payload, schema_changed)` so the
        // notify branch below can fire correctly for merges that ran
        // an `ALTER TABLE ADD COLUMN`. `replace` always changes shape;
        // `merge` only does conditionally; `append` never does.
        let result = self.with_engine(|engine| {
            let target_db =
                self.resolve_db(engine, params.database.as_deref(), params.persist, true)?;
            crate::attach::validate_input_path(&params.path, "data file")?;
            let schema_override = crate::schema::normalize_schema_param(params.schema.as_ref())?;
            let opts = IngestOptions {
                table: params.table.clone(),
                mode: mode.clone(),
                schema_override,
                merge_key: merge_key_vec.clone(),
                target_db: target_db.clone(),
            };

            let ingest_result = if let Some(ref json_path) = params.json_extract_path {
                let raw = std::fs::read_to_string(&params.path).map_err(|e| {
                    McpError::new(
                        ErrorCode::FileNotFound,
                        format!("Cannot read file '{}': {e}", params.path),
                    )
                })?;
                let extracted = crate::ingest::extract_json_path(&raw, json_path)?;
                let array_text = crate::ingest::normalize_json_or_jsonl(&extracted)?;
                let mut result = ingest_json(engine, &array_text, &opts)?;
                result.stats.operation = "load_file".into();
                result.stats.bytes_read = std::fs::metadata(&params.path).map_or(0, |m| m.len());
                result.stats.file_format = Some("json".into());
                result
            } else {
                match detect_file_format(std::path::Path::new(&params.path)) {
                    InferredFileFormat::Parquet => ingest_parquet_file(engine, &params.path, &opts),
                    InferredFileFormat::ArrowIpc => {
                        ingest_arrow_ipc_file(engine, &params.path, &opts)
                    }
                    InferredFileFormat::Json => ingest_json_file(engine, &params.path, &opts),
                    InferredFileFormat::Csv => ingest_csv_file(engine, &params.path, &opts),
                }?
            };

            // Capture the schema-changed flag before consuming
            // `ingest_result` so the closure can return it alongside
            // the JSON payload.
            let schema_changed = ingest_result.stats.schema_changed;

            let schema_json: Vec<Value> = ingest_result
                .schema
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name,
                        "type": c.hyper_type,
                        "nullable": c.nullable,
                    })
                })
                .collect();

            // Catalog: helper routes to target_db's per-DB catalog.
            {
                let load_params = serde_json::to_string(&json!({
                    "source_path": params.path,
                    "mode": mode,
                    "schema": params.schema,
                    "json_extract_path": params.json_extract_path,
                    "merge_key": merge_key_vec,
                    "database": target_db.as_deref().unwrap_or("local"),
                }))
                .ok();
                self.after_ingest_catalog_update(
                    engine,
                    &params.table,
                    "load_file",
                    load_params.as_deref(),
                    i64::try_from(ingest_result.rows).ok(),
                    target_db.as_deref(),
                );
            }

            Ok((
                json!({
                    "rows": ingest_result.rows,
                    "schema": schema_json,
                    "stats": ingest_result.stats.to_json(),
                }),
                schema_changed,
            ))
        });

        match result {
            Ok((val, schema_changed)) => {
                self.notify_table_changed(&table_name);
                // Notify when the resource list's *shape* actually
                // changed: `replace` always (table dropped/recreated),
                // and `merge` only when it ran an `ALTER TABLE ADD
                // COLUMN` (or created the target via the rename short-
                // circuit). A merge that only updated existing rows
                // leaves the schema untouched, so we skip the
                // broadcast — same precedent as `append`.
                if mode == "replace" || schema_changed {
                    self.notify_resource_list_changed();
                }
                Self::ok_content(val)
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Ingest multiple files in parallel across a pool of async connections.
    /// Each entry behaves like a standalone `load_file` call; failures are
    /// reported per-file rather than aborting the whole batch.
    #[tool(
        description = "Ingest multiple files in parallel. Each entry is equivalent to a standalone `load_file` call (same formats and same format-selection guidance: prefer Parquet > CSV > Arrow IPC > JSON for large imports). The batch runs across a pool of async connections sized by `concurrency` (default `min(files.len(), 8)`), so independent files finish roughly in max-time rather than sum-time. Per-file errors are captured in the response and do not abort the rest of the batch; the top-level call still returns Ok. For Apache Iceberg tables, call `load_iceberg` per table instead — this tool only handles single-file formats.\n\nUse `database` (or shorthand `persist: true`) to target a non-primary database; the same value applies to every entry in the batch. **Note: `mode = \"merge\"` is not supported here — use `load_file` once per file when you need merge/upsert semantics.**"
    )]
    fn load_files(
        &self,
        Parameters(params): Parameters<LoadFilesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        use hyperdb_api::pool::{create_pool, PoolConfig};
        use hyperdb_api::CreateMode;

        if let Err(e) = self.check_writable("load_files") {
            return Self::err_content(e);
        }
        if params.files.is_empty() {
            return Self::err_content(McpError::new(
                ErrorCode::EmptyData,
                "load_files: `files` must not be empty",
            ));
        }

        // Reject `mode = "merge"` (or stray `merge_key`) up front, before
        // we spin up the connection pool and dispatch the parallel batch.
        // The async ingest paths driven from this batch loader don't
        // carry the merge-via-temp-table branch, and rejecting per-entry
        // would produce a confusing N-rejection result for a uniform
        // merge call. One top-level error is the clearer contract.
        for (idx, entry) in params.files.iter().enumerate() {
            if let Err(mut e) = crate::attach::validate_input_path(&entry.path, "data file") {
                e.message = format!("entry {idx} (table '{}'): {}", entry.table, e.message);
                return Self::err_content(e);
            }
            let mode = entry.mode.as_deref().unwrap_or("replace");
            if mode == "merge" || entry.merge_key.is_some() {
                return Self::err_content(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "load_files does not support mode=merge yet (entry {idx}, table \
                         '{}'). Call load_file once per file when you need merge semantics.",
                        entry.table
                    ),
                ));
            }
        }

        // Resolve hyperd endpoint + the workspace path matching the
        // resolved target database. The pool opens that .hyper file
        // directly (under the same alias the engine uses) so qualified
        // SQL routes correctly. Read-only attachments are rejected by
        // resolve_db.
        let (endpoint, workspace, target_db) = match self.with_engine(|engine| {
            let target_db =
                self.resolve_db(engine, params.database.as_deref(), params.persist, true)?;
            let endpoint = engine.hyperd_endpoint()?;
            let workspace = match target_db.as_deref() {
                None => engine.ephemeral_path().to_string_lossy().to_string(),
                Some(alias) if alias.eq_ignore_ascii_case(Engine::PERSISTENT_ALIAS) => engine
                    .persistent_path()
                    .ok_or_else(|| {
                        McpError::new(
                            ErrorCode::InvalidArgument,
                            "target 'persistent' but the server is in --ephemeral-only mode",
                        )
                    })?
                    .to_string_lossy()
                    .to_string(),
                Some(alias) => {
                    let entry = self.attachments.get(alias).ok_or_else(|| {
                        McpError::new(
                            ErrorCode::InvalidArgument,
                            format!("database '{alias}' is not attached"),
                        )
                    })?;
                    let crate::attach::AttachSource::LocalFile { path } = &entry.source;
                    path.to_string_lossy().to_string()
                }
            };
            Ok((endpoint, workspace, target_db))
        }) {
            Ok(v) => v,
            Err(e) => return Self::err_content(e),
        };

        // Pool size: cap at files.len() and an absolute ceiling of 16 to
        // avoid starving the primary connection hyperd is already servicing.
        let file_count = params.files.len();
        let concurrency = params
            .concurrency
            .map_or(8, |n| n as usize)
            .min(file_count)
            .clamp(1, 16);

        let pool = match create_pool(
            PoolConfig::new(endpoint, workspace)
                .create_mode(CreateMode::DoNotCreate)
                .max_size(concurrency),
        ) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                return Self::err_content(McpError::new(
                    ErrorCode::InternalError,
                    format!("Failed to build connection pool for load_files: {e}"),
                ))
            }
        };

        // Drive the async fan-out from this sync tool handler using the
        // same pattern as `start_watching`: block_in_place + block_on.
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return Self::err_content(McpError::new(
                ErrorCode::InternalError,
                "load_files must run inside a tokio runtime",
            ));
        };

        // Per-entry result payload. Successful entries carry rows/schema/stats;
        // failures carry error code + message. Order matches input `files`.
        #[derive(Default)]
        struct EntryOutcome {
            table: String,
            ok: Option<(u64, Vec<Value>, Value)>,
            err: Option<(ErrorCode, String)>,
            replace_mode: bool,
        }

        let outcomes: Vec<EntryOutcome> = tokio::task::block_in_place(|| {
            rt.block_on(async {
                let mut set = tokio::task::JoinSet::new();
                for (idx, entry) in params.files.into_iter().enumerate() {
                    let pool = Arc::clone(&pool);
                    let entry_target_db = target_db.clone();
                    set.spawn(async move {
                        let mode = entry.mode.clone().unwrap_or_else(|| "replace".into());
                        let replace_mode = mode == "replace";
                        let mut out = EntryOutcome {
                            table: entry.table.clone(),
                            replace_mode,
                            ..Default::default()
                        };

                        // `merge` mode is rejected up front in the
                        // top-level handler; per-entry guard would be
                        // dead code here.

                        let schema_override =
                            match crate::schema::normalize_schema_param(entry.schema.as_ref()) {
                                Ok(v) => v,
                                Err(e) => {
                                    out.err = Some((e.code, e.message));
                                    return (idx, out);
                                }
                            };
                        // The pool was built against the resolved target's
                        // .hyper file as its workspace, so from these
                        // connections' perspective the target IS the primary
                        // database. Keep target_db unqualified (None) so SQL
                        // routes into the pool's primary instead of trying
                        // to qualify against an alias that doesn't exist on
                        // these connections. The `entry_target_db` is still
                        // used downstream for the catalog gate.
                        let _ = &entry_target_db;
                        let opts = IngestOptions {
                            table: entry.table.clone(),
                            mode: mode.clone(),
                            schema_override,
                            merge_key: None,
                            target_db: None,
                        };

                        // Check out a connection from the pool. Held only
                        // for the duration of this one ingest, then released.
                        let conn = match pool.get().await {
                            Ok(c) => c,
                            Err(e) => {
                                out.err = Some((
                                    ErrorCode::InternalError,
                                    format!("Failed to check out connection: {e}"),
                                ));
                                return (idx, out);
                            }
                        };

                        // `json_extract_path` only makes sense for JSON; the
                        // sync loader wraps the file read + normalize step
                        // around `ingest_json`. Mirror that here using the
                        // async ingest_json on the pooled connection.
                        let ingest_res = if let Some(ref json_path) = entry.json_extract_path {
                            let raw = match std::fs::read_to_string(&entry.path) {
                                Ok(s) => s,
                                Err(e) => {
                                    out.err = Some((
                                        ErrorCode::FileNotFound,
                                        format!("Cannot read file '{}': {e}", entry.path),
                                    ));
                                    return (idx, out);
                                }
                            };
                            let extracted = match crate::ingest::extract_json_path(&raw, json_path)
                            {
                                Ok(v) => v,
                                Err(e) => {
                                    out.err = Some((e.code, e.message));
                                    return (idx, out);
                                }
                            };
                            let array_text =
                                match crate::ingest::normalize_json_or_jsonl(&extracted) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        out.err = Some((e.code, e.message));
                                        return (idx, out);
                                    }
                                };
                            crate::ingest::ingest_json_async(&conn, &array_text, &opts)
                                .await
                                .map(|mut r| {
                                    r.stats.operation = "load_file".into();
                                    r.stats.bytes_read =
                                        std::fs::metadata(&entry.path).map_or(0, |m| m.len());
                                    r.stats.file_format = Some("json".into());
                                    r
                                })
                        } else {
                            match detect_file_format(std::path::Path::new(&entry.path)) {
                                InferredFileFormat::Parquet => {
                                    ingest_parquet_file_async(&conn, &entry.path, &opts).await
                                }
                                InferredFileFormat::ArrowIpc => {
                                    ingest_arrow_ipc_file_async(&conn, &entry.path, &opts).await
                                }
                                InferredFileFormat::Json => {
                                    ingest_json_file_async(&conn, &entry.path, &opts).await
                                }
                                InferredFileFormat::Csv => {
                                    ingest_csv_file_async(&conn, &entry.path, &opts).await
                                }
                            }
                        };

                        match ingest_res {
                            Ok(r) => {
                                let schema_json: Vec<Value> = r
                                    .schema
                                    .iter()
                                    .map(|c| {
                                        json!({
                                            "name": c.name,
                                            "type": c.hyper_type,
                                            "nullable": c.nullable,
                                        })
                                    })
                                    .collect();
                                out.ok = Some((r.rows, schema_json, r.stats.to_json()));
                            }
                            Err(e) => {
                                out.err = Some((e.code, e.message));
                            }
                        }

                        (idx, out)
                    });
                }

                // Preserve input order when flattening the join set so the
                // response mirrors the caller's `files` array 1-for-1.
                let mut collected: Vec<Option<EntryOutcome>> =
                    (0..file_count).map(|_| None).collect();
                while let Some(joined) = set.join_next().await {
                    match joined {
                        Ok((idx, outcome)) => collected[idx] = Some(outcome),
                        Err(e) => {
                            // A task panicked — surface it as an error on a
                            // synthetic slot so the caller sees something.
                            tracing::warn!("load_files task join error: {e}");
                        }
                    }
                }
                collected.into_iter().flatten().collect()
            })
        });

        // Catalog bookkeeping + notifications for successful loads. Runs
        // back on the sync engine connection. Best-effort; errors are
        // logged but don't fail the batch response.
        let mut any_replace_succeeded = false;
        let mut tables_to_notify: Vec<String> = Vec::new();
        let results_json: Vec<Value> = outcomes
            .iter()
            .map(|o| match (&o.ok, &o.err) {
                (Some((rows, schema, stats)), _) => {
                    tables_to_notify.push(o.table.clone());
                    if o.replace_mode {
                        any_replace_succeeded = true;
                    }
                    json!({
                        "table": o.table,
                        "rows": rows,
                        "schema": schema,
                        "stats": stats,
                    })
                }
                (None, Some((code, msg))) => json!({
                    "table": o.table,
                    "error": {
                        "code": format!("{:?}", code),
                        "message": msg,
                    }
                }),
                // Shouldn't happen (exactly one of ok/err is set) but be
                // defensive — emit a placeholder rather than panicking.
                (None, None) => json!({
                    "table": o.table,
                    "error": {
                        "code": "InternalError",
                        "message": "load_files task produced no outcome",
                    }
                }),
            })
            .collect();

        // Update the per-table catalog stubs for every success. Requires
        // the engine, so we run this inside `with_engine`. The helper
        // routes the upsert to target_db's per-DB _table_catalog.
        if let Err(e) = self.with_engine(|engine| {
            for o in &outcomes {
                if let Some((rows, _, _)) = &o.ok {
                    self.after_ingest_catalog_update(
                        engine,
                        &o.table,
                        "load_file",
                        None,
                        i64::try_from(*rows).ok(),
                        target_db.as_deref(),
                    );
                }
            }
            Ok(())
        }) {
            tracing::warn!("load_files: catalog update batch failed: {}", e.message);
        }

        for t in &tables_to_notify {
            self.notify_table_changed(t);
        }
        if any_replace_succeeded {
            self.notify_resource_list_changed();
        }

        let success_count = outcomes.iter().filter(|o| o.ok.is_some()).count();
        let failure_count = outcomes.len() - success_count;

        Self::ok_content(json!({
            "results": results_json,
            "summary": {
                "total": outcomes.len(),
                "succeeded": success_count,
                "failed": failure_count,
                "concurrency": concurrency,
            }
        }))
    }

    /// Ingest an Apache Iceberg table directory into a workspace table
    /// using hyperd's native `external(..., format => 'iceberg')` reader.
    #[tool(
        description = "Ingest an Apache Iceberg table into a workspace table using hyperd's native Iceberg reader. `path` must be an absolute path to the Iceberg table *root directory* (the one containing the `metadata/` and `data/` subdirs). Hyperd resolves the latest snapshot by default; pass `metadata_filename` (e.g. `v2.metadata.json`) or `version_as_of` to pin a specific snapshot or version. Mode is `replace` (default) or `append`. Single SQL statement under the hood — no Rust-side Arrow decode, no per-row INSERTs."
    )]
    fn load_iceberg(
        &self,
        Parameters(params): Parameters<LoadIcebergParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("load_iceberg") {
            return Self::err_content(e);
        }
        let table_name = params.table.clone();
        let mode = params.mode.clone().unwrap_or_else(|| "replace".into());
        let opts = crate::lakehouse::IcebergIngestOptions {
            table: params.table.clone(),
            mode: mode.clone(),
            metadata_filename: params.metadata_filename.clone(),
            version_as_of: params.version_as_of,
        };

        let result = self.with_engine(|engine| {
            // Iceberg "path" is a directory, not a file — validate as input path.
            crate::attach::validate_input_path(&params.path, "iceberg table")?;
            let ingest_result =
                crate::lakehouse::ingest_iceberg_table(engine, &params.path, &opts)?;

            let schema_json: Vec<Value> = ingest_result
                .schema
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name,
                        "type": c.hyper_type,
                        "nullable": c.nullable,
                    })
                })
                .collect();

            let load_params = serde_json::to_string(&json!({
                "source_path": params.path,
                "mode": mode,
                "format": "iceberg",
                "metadata_filename": params.metadata_filename,
                "version_as_of": params.version_as_of,
            }))
            .ok();
            self.after_ingest_catalog_update(
                engine,
                &params.table,
                "load_iceberg",
                load_params.as_deref(),
                i64::try_from(ingest_result.rows).ok(),
                None,
            );

            Ok(json!({
                "rows": ingest_result.rows,
                "schema": schema_json,
                "stats": ingest_result.stats.to_json(),
            }))
        });

        match result {
            Ok(val) => {
                self.notify_table_changed(&table_name);
                if mode == "replace" {
                    self.notify_resource_list_changed();
                }
                Self::ok_content(val)
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Run a read-only SQL query (SELECT, WITH, EXPLAIN, SHOW, VALUES).
    #[tool(
        description = "Run a read-only SQL query (SELECT, WITH, EXPLAIN, SHOW, VALUES) against the workspace. For DDL/DML use the execute tool."
    )]
    fn query(
        &self,
        Parameters(params): Parameters<QueryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(|engine| {
            if !is_read_only_sql(&params.sql) {
                return Err(McpError::new(
                    ErrorCode::SqlError,
                    "The query tool only accepts read-only SQL (SELECT, WITH, EXPLAIN, SHOW, VALUES). Use the execute tool for DDL/DML.",
                ));
            }
            // Optional database routing — temporarily redirect search_path
            // for the duration of this call. Restored on guard drop.
            let target_db = self.resolve_db(engine, params.database.as_deref(), None, false)?;
            let _search_guard = match target_db {
                Some(ref alias) => Some(engine.scoped_search_path(alias)?),
                None => None,
            };
            // Cap result-set size sent back to the LLM. Larger result sets blow
            // the model's context window and stall the conversation. Users who
            // need full scans should use `export` to write to a file.
            const MAX_QUERY_ROWS: usize = 10_000;

            let timer = crate::stats::StatsTimer::start();
            let mut rows = engine.execute_query_to_json(&params.sql)?;
            let total_rows = rows.len();
            let truncated = total_rows > MAX_QUERY_ROWS;
            if truncated {
                rows.truncate(MAX_QUERY_ROWS);
            }
            let elapsed = timer.elapsed_ms();
            let stats = crate::stats::QueryStats {
                operation: "query".into(),
                rows_returned: rows.len() as u64,
                rows_scanned: 0,
                elapsed_ms: elapsed,
                result_size_bytes: serde_json::to_string(&rows).map_or(0, |s| s.len() as u64),
                tables_touched: vec![],
            };
            let payload = if truncated {
                json!({
                    "result": rows,
                    "stats": stats.to_json(),
                    "truncated": true,
                    "total_rows": total_rows,
                    "rows_returned": MAX_QUERY_ROWS,
                    "hint": format!(
                        "Result set has {total_rows} rows; only the first {MAX_QUERY_ROWS} \
                         are shown. Add a LIMIT clause, aggregate with GROUP BY, or use \
                         the `export` tool to write the full result to a file."
                    ),
                })
            } else {
                json!({
                    "result": rows,
                    "stats": stats.to_json(),
                })
            };
            Ok((params.sql.clone(), payload))
        });

        match result {
            Ok((sql, val)) => {
                let formatted_sql = Self::fmt_sql(&sql);
                let json_text = serde_json::to_string_pretty(&val).unwrap_or_default();
                Ok(CallToolResult::success(vec![
                    Content::text(format!("```sql\n{formatted_sql}\n```")),
                    Content::text(json_text),
                ]))
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Execute a DDL or DML statement (CREATE, INSERT, UPDATE, DELETE, DROP, etc.).
    #[tool(
        description = "Execute a DDL/DML statement (CREATE TABLE, INSERT, UPDATE, DELETE, DROP, ALTER, COPY, etc.). Returns affected row count. Disabled in read-only mode."
    )]
    fn execute(
        &self,
        Parameters(params): Parameters<ExecuteParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("execute") {
            return Self::err_content(e);
        }
        let sql = params.sql.clone();
        let result = self.with_engine(|engine| {
            if is_read_only_sql(&params.sql) {
                return Err(McpError::new(
                    ErrorCode::SqlError,
                    "The execute tool is for DDL/DML. Use the query tool for SELECT/WITH/EXPLAIN statements.",
                ));
            }
            // Optional database routing — temporarily redirect search_path.
            // require_writable=true ensures non-primary aliases must be writable.
            let target_db = self.resolve_db(engine, params.database.as_deref(), None, true)?;
            let _search_guard = match target_db {
                Some(ref alias) => Some(engine.scoped_search_path(alias)?),
                None => None,
            };
            let timer = crate::stats::StatsTimer::start();
            let affected = engine.execute_command(&params.sql)?;
            let elapsed = timer.elapsed_ms();
            // Reconcile only when the statement could have changed the
            // set of tables (CREATE / DROP / ALTER / TRUNCATE / RENAME).
            // INSERT / UPDATE / DELETE can't add or remove tables, so
            // running `reconcile_in` on every row-level execute would
            // do `2N + 2` SQL round-trips of pure waste — and after
            // M4's M-target fan-out, `4N + 4` for user-attached
            // targets. Same gate as `notify_resource_list_changed`
            // below; both fire on the same set of statements.
            //
            // Threads `target_db` so a structural DDL against a
            // user-attached alias also reconciles that DB's catalog
            // (otherwise the dropped table's row stays stranded —
            // bootstrap reconcile only walks persistent).
            if is_structural_sql(&params.sql) {
                self.after_execute_catalog_update(engine, target_db.as_deref());
            }
            Ok(json!({
                "sql": Self::fmt_sql(&params.sql),
                "affected_rows": affected,
                "stats": { "operation": "command", "elapsed_ms": elapsed },
            }))
        });

        match result {
            Ok(val) => {
                // Arbitrary DDL/DML may have touched any table — fire the
                // workspace-wide summary updates, and a list_changed to
                // nudge subscribers to refresh their resource catalog for
                // CREATE / DROP style statements.
                self.notify_workspace_changed();
                if is_structural_sql(&sql) {
                    self.notify_resource_list_changed();
                }
                Self::ok_content(val)
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Return the schema, total row count, and the first N rows of a table.
    #[tool(
        description = "Return the schema, total row count, and first N rows of a table. Combines describe + sample query in one call. N defaults to 5, max 100."
    )]
    fn sample(
        &self,
        Parameters(params): Parameters<SampleParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(|engine| {
            let target_db = self.resolve_db(engine, params.database.as_deref(), None, false)?;
            let timer = crate::stats::StatsTimer::start();
            let n = params.n.unwrap_or(5);
            let mut sample = engine.sample_table_in(target_db.as_deref(), &params.table, n)?;
            let elapsed = timer.elapsed_ms();
            if let Some(obj) = sample.as_object_mut() {
                obj.insert(
                    "stats".into(),
                    json!({ "operation": "sample", "elapsed_ms": elapsed }),
                );
            }
            Ok(sample)
        });

        match result {
            Ok(val) => Self::ok_content(val),
            Err(e) => Self::err_content(e),
        }
    }

    /// Render a chart (PNG or SVG) from a SQL query.
    #[tool(
        description = "Render a chart (bar, line, scatter, or histogram) from a SQL query. Writes the image to disk by default and returns a short stats blob with the path — use `Read(path)` to display it (this keeps the MCP transcript small). Set `inline=true` to also receive the PNG/SVG bytes inline in the tool result; combine with `output_path` to get both.\n\n- `output_path`: explicit destination file path. Parent directory is created automatically. If omitted, a file is auto-generated under the system temp dir as `hyperdb-charts/chart-<ts>-<pid>-<n>.<ext>`.\n- `inline`: when true, return the image bytes inline. Without `output_path`, suppresses the disk write entirely. With `output_path`, writes to disk AND returns inline. Defaults to false.\n- `format`: \"png\" (default) or \"svg\". Auto-derived from `output_path` extension when omitted. A mismatch between `format` and the path extension returns `INVALID_ARGUMENT`.\n- `overwrite`: default true. Set false to refuse overwriting an existing file (returns `PERMISSION_DENIED`).\n- `x_range` / `y_range`: fix axis extents across multiple charts (e.g. x_range=[0,1500], y_range=[0,1]).\n- `color_map`: stable per-series hex colors (e.g. {\"India\":\"#e41a1c\",\"China\":\"#ff7f0e\"}).\n- `label_points=true`: annotate each point with its series name instead of showing a legend — best when each series has exactly one point."
    )]
    fn chart(
        &self,
        Parameters(params): Parameters<ChartParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(|engine| {
            if !is_read_only_sql(&params.sql) {
                return Err(McpError::new(
                    ErrorCode::SqlError,
                    "The chart tool only accepts read-only SQL (SELECT, WITH, EXPLAIN, SHOW, VALUES).",
                ));
            }

            // If the caller passed an explicit output path, validate it.
            // Auto-generated paths land in a temp dir and don't need this gate.
            if let Some(out) = params.output_path.as_deref() {
                crate::attach::validate_output_path(out, "chart output")?;
            }
            // Resolve format up front — the path extension may imply it,
            // and we need the format before we can auto-generate a path.
            let format = crate::chart::resolve_chart_format(
                params.format.as_deref(),
                params.output_path.as_deref(),
            )?;

            // Optional database routing — temporarily redirect search_path
            // so unqualified names in the chart SQL resolve there.
            let target_db = self.resolve_db(engine, params.database.as_deref(), None, false)?;
            let _search_guard = match target_db {
                Some(ref alias) => Some(engine.scoped_search_path(alias)?),
                None => None,
            };

            let timer = crate::stats::StatsTimer::start();
            let rows = engine.execute_query_to_json(&params.sql)?;

            // Parse color_map: skip entries whose hex string is malformed,
            // logging them via the description rather than hard-failing.
            let color_map = params
                .color_map
                .as_ref()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| {
                            crate::chart::parse_hex_color(v)
                                .map(|c| (k.clone(), c))
                        })
                        .collect::<std::collections::HashMap<_, _>>()
                })
                .unwrap_or_default();

            let opts = ChartOptions {
                chart_type: ChartType::parse(&params.chart_type)?,
                x_column: params.x.clone(),
                y_column: params.y.clone(),
                series_column: params.series.clone(),
                title: params.title.clone(),
                format,
                width: params.width.unwrap_or(800).clamp(200, 4096),
                height: params.height.unwrap_or(480).clamp(150, 4096),
                bins: params.bins.unwrap_or(20).clamp(1, 500),
                x_as_category: params.x_as_category,
                x_range: params.x_range,
                y_range: params.y_range,
                color_map,
                label_points: params.label_points.unwrap_or(false),
            };

            let chart = render_chart(&rows, &opts)?;

            // Decide disk vs inline vs both. Write to disk *before*
            // building the content vec so an I/O failure surfaces as a
            // tool error instead of a half-delivered response.
            let disposition = crate::chart::resolve_chart_disposition(
                params.inline.unwrap_or(false),
                params.output_path.as_deref(),
                opts.format,
            );
            let overwrite = params.overwrite.unwrap_or(true);
            if let Some(path) = disposition.path() {
                crate::chart::write_chart_to_disk(path, &chart.bytes, overwrite)?;
            }

            let elapsed = timer.elapsed_ms();
            Ok((chart, elapsed, opts, disposition))
        });

        match result {
            Ok((chart, elapsed_ms, opts, disposition)) => {
                let format_str = match opts.format {
                    ChartFormat::Png => "png",
                    ChartFormat::Svg => "svg",
                };
                let wants_inline = disposition.wants_inline();
                let output_path_str = disposition.path().map(|p| p.to_string_lossy().into_owned());

                let mut stats = serde_json::Map::new();
                stats.insert("operation".into(), json!("chart"));
                stats.insert("rows_plotted".into(), json!(chart.rows_plotted));
                stats.insert("elapsed_ms".into(), json!(elapsed_ms));
                stats.insert("format".into(), json!(format_str));
                stats.insert("bytes".into(), json!(chart.bytes.len()));
                stats.insert("width".into(), json!(opts.width));
                stats.insert("height".into(), json!(opts.height));
                stats.insert("inline".into(), json!(wants_inline));
                if let Some(p) = output_path_str {
                    stats.insert("output_path".into(), json!(p));
                }
                let stats_text =
                    serde_json::to_string_pretty(&Value::Object(stats)).unwrap_or_default();

                let mut content = Vec::with_capacity(2);
                if wants_inline {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&chart.bytes);
                    content.push(Content::image(b64, chart.mime_type.to_string()));
                }
                content.push(Content::text(stats_text));
                Ok(CallToolResult::success(content))
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Begin watching a directory for `.ready` sentinel files. See
    /// [`crate::watcher`] for the full producer/consumer protocol.
    #[tool(
        description = "Watch a directory for files to auto-ingest. Producers write data file + companion <name>.ready sentinel; the watcher appends the data file to the given table and deletes both on success. Use `database` (or shorthand `persist: true`) to target a non-primary database — the watcher's connection pool opens that file directly. `detach_database` rejects while a watcher is active; call `unwatch_directory` first. Disabled in read-only mode."
    )]
    fn watch_directory(
        &self,
        Parameters(params): Parameters<WatchDirectoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("watch_directory") {
            return Self::err_content(e);
        }
        let canonical = match crate::attach::validate_input_path(&params.path, "watch directory") {
            Ok(p) => p,
            Err(e) => return Self::err_content(e),
        };
        // Eagerly initialize the engine so the background watcher thread can
        // assume `engine.as_ref()` is Some without needing workspace_path.
        match self.ensure_engine() {
            Ok(guard) => drop(guard),
            Err(e) => return Self::err_content(e),
        }

        // Resolve the target database once, under the engine lock. Read-only
        // attachments are rejected here (require_writable=true) so the
        // watcher can't be pointed at a destination it can't write to.
        let target_db = match self.with_engine(|engine| {
            self.resolve_db(engine, params.database.as_deref(), params.persist, true)
        }) {
            Ok(v) => v,
            Err(e) => return Self::err_content(e),
        };

        let path = canonical;
        let engine_handle = self.engine_handle();
        let attachments = self.attachments_handle();
        let registry = self.watchers_handle();
        let options = crate::watcher::WatchOptions {
            max_concurrent: params.max_concurrent.unwrap_or(0) as usize,
        };
        let result = crate::watcher::start_watching(
            engine_handle,
            attachments,
            registry,
            Some(self.subscriptions_handle()),
            path.clone(),
            params.table.clone(),
            target_db,
            options,
        );
        match result {
            Ok(stats) => {
                let body = json!({
                    "directory": path.to_string_lossy(),
                    "table": params.table,
                    "status": "watching",
                    "max_concurrent": stats.max_concurrent,
                    "initial_sweep": {
                        "files_ingested": stats.files_ingested,
                        "files_failed": stats.files_failed,
                    },
                });
                Self::ok_content(body)
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Stop watching a directory.
    #[tool(
        description = "Stop watching a directory previously registered with watch_directory. Pending .ready files are left in place."
    )]
    fn unwatch_directory(
        &self,
        Parameters(params): Parameters<UnwatchDirectoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let path = std::path::PathBuf::from(&params.path);
        let result = crate::watcher::stop_watching(&self.watchers_handle(), &path);
        match result {
            Ok(summary) => Self::ok_content(summary),
            Err(e) => Self::err_content(e),
        }
    }

    /// Describe workspace tables. With `table` set, returns just that
    /// table's columns and row count; without it, lists every public table.
    #[tool(
        description = "Describe workspace tables. With `table` set, returns that single table's columns and row count (TABLE_NOT_FOUND if missing). Without `table`, lists every public table."
    )]
    fn describe(
        &self,
        Parameters(params): Parameters<DescribeParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(|engine| {
            let target_db = self.resolve_db(engine, params.database.as_deref(), None, false)?;
            match params.table.as_deref() {
                Some(name) => engine
                    .describe_table_in(target_db.as_deref(), name)
                    .map(|t| vec![t]),
                None => engine.describe_tables_in(target_db.as_deref()),
            }
        });

        match result {
            Ok(tables) => Self::ok_content(json!({"tables": tables})),
            Err(e) => Self::err_content(e),
        }
    }

    /// Dry-run schema inference on a file (CSV, Parquet, Arrow IPC) without
    /// ingesting it. Returns the inferred schema plus per-column diagnostics
    /// (`null_count`, `min`, `max`, `sample_values`) so an LLM can construct
    /// a safer `schema` override for `load_file` / `load_data`.
    #[tool(
        description = "Dry-run schema inference on a CSV / Parquet / Arrow IPC file without ingesting. Returns the schema load_file would use (including the full-file numeric widening pass), plus per-column null_count, min, max, and sample_values. Use this BEFORE load_file if you are unsure about types or ran into a SchemaMismatch / numeric overflow — then pass an explicit `schema` override on the subsequent load_file call. Use `json_extract_path` to inspect a nested data array inside a JSON wrapper file (e.g., MCP tool responses saved to disk)."
    )]
    #[expect(
        clippy::unused_self,
        reason = "method retained on the type for API symmetry; implementation currently does not need state"
    )]
    fn inspect_file(
        &self,
        Parameters(params): Parameters<InspectFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = crate::attach::validate_input_path(&params.path, "data file") {
            return Self::err_content(e);
        }
        let sample_rows = params.sample_rows.unwrap_or(5).clamp(1, 50) as usize;
        let result = if let Some(ref json_path) = params.json_extract_path {
            (|| -> Result<_, McpError> {
                let raw = std::fs::read_to_string(&params.path).map_err(|e| {
                    McpError::new(
                        ErrorCode::FileNotFound,
                        format!("Cannot read file '{}': {e}", params.path),
                    )
                })?;
                let file_size = std::fs::metadata(&params.path).map_or(0, |m| m.len());
                let extracted = crate::ingest::extract_json_path(&raw, json_path)?;
                crate::inspect::inspect_json_from_text(&extracted, file_size, sample_rows)
            })()
        } else {
            crate::inspect::inspect_source(&params.path, sample_rows)
        };
        match result {
            Ok(report) => Self::ok_content(report.to_json()),
            Err(e) => Self::err_content(e),
        }
    }

    /// Export query results or a table to CSV, Parquet, Arrow IPC,
    /// Apache Iceberg, or a new `.hyper` file.
    #[tool(
        description = "Export query results or a table to a file via hyperd's native writers. Every format listed here is server-side — hyperd writes the file directly, with zero per-row work in the MCP process — and every format round-trips cleanly through the matching loader (`load_file` or `load_iceberg`).\n\nWhen choosing a format for *data leaving* Hyper, prefer in this order:\n  1. **Parquet** (recommended default): smallest output, fastest write, preserves every type (NUMERIC precision/scale, DATE, TIMESTAMP, etc.). `path` is a single file.\n  2. **Iceberg**: produces a full Apache Iceberg table directory (`metadata/` + `data/`). Use when the consumer is a data-lake tool (Spark, Trino, DuckDB, etc.). `path` is a directory that hyperd creates.\n  3. **Arrow IPC Stream** (`arrow_ipc`): same wire shape Hyper uses internally; great for handing data to another Arrow-aware process. Larger than Parquet (no compression) but extremely fast to read back. `path` is a single file.\n  4. **CSV**: portable and human-readable but the largest output and types are lost (everything becomes text). Use for spreadsheet / shell-pipeline interop. Includes header row.\n  5. **Hyper**: an entire `.hyper` database file openable directly in Tableau Desktop. `sql`/`table` are ignored — every user table is copied.\n\nAll formats except Iceberg and Hyper require either `sql` or `table`. Iceberg output is a directory; all others are single files.\n\nUse `database` to read from a non-primary source: for `format=\"hyper\"` it selects which database is snapshotted; for the row-oriented formats it routes the SELECT through the named database (when `table` is set) or pins `schema_search_path` for the call (when `sql` is set)."
    )]
    fn export(
        &self,
        Parameters(params): Parameters<ExportParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(|engine| {
            // Validate output path: must be absolute, no `..` components.
            // (Iceberg "exports" to a directory; the same rules apply.)
            crate::attach::validate_output_path(&params.path, "export")?;
            // `format_options` must be a JSON object if supplied. Anything
            // else (array, string, number, null) is a caller error — reject
            // with a clear message rather than silently dropping it.
            let format_options = match params.format_options.clone() {
                None => None,
                Some(Value::Object(m)) => Some(m),
                Some(other) => {
                    return Err(McpError::new(
                        ErrorCode::SchemaMismatch,
                        format!("export: format_options must be a JSON object, got: {other}"),
                    ));
                }
            };
            // Database routing. Three strategies:
            // - `hyper` format + non-primary: source_db plumbed through
            //   into populate_export_target so the snapshot reads from
            //   the requested database (no need to redirect anything;
            //   the cross-DB CREATE TABLE AS handles it natively).
            // - `table` mode + non-primary: synthesize a fully-qualified
            //   SELECT and pass it as `sql` so export.rs's name-quoting
            //   doesn't double-quote our identifier.
            // - `sql` mode + non-primary: redirect search_path for the
            //   call duration so unqualified names resolve correctly.
            let target_db = self.resolve_db(engine, params.database.as_deref(), None, false)?;
            let (effective_sql, effective_table) = match (&params.sql, &params.table, &target_db) {
                (None, Some(t), Some(db)) => {
                    let esc_db = db.replace('"', "\"\"");
                    let esc_tbl = t.replace('"', "\"\"");
                    (
                        Some(format!(
                            "SELECT * FROM \"{esc_db}\".\"public\".\"{esc_tbl}\""
                        )),
                        None,
                    )
                }
                _ => (params.sql.clone(), params.table.clone()),
            };
            let _search_guard = match (&effective_sql, &target_db, &params.sql) {
                // Only pin search_path when the user supplied raw SQL
                // (not when we synthesized a fully-qualified SELECT).
                (Some(_), Some(alias), Some(_)) => Some(engine.scoped_search_path(alias)?),
                _ => None,
            };
            let opts = ExportOptions {
                sql: effective_sql,
                table: effective_table,
                path: params.path,
                format: params.format,
                overwrite: params.overwrite.unwrap_or(true),
                format_options,
                source_db: target_db.clone(),
            };
            let export_result = export_to_file(engine, &opts)?;
            Ok(json!({
                "output_path": export_result.stats.output_path,
                "rows": export_result.rows,
                "file_size_bytes": export_result.stats.file_size_bytes,
                "stats": export_result.stats.to_json(),
            }))
        });

        match result {
            Ok(val) => Self::ok_content(val),
            Err(e) => Self::err_content(e),
        }
    }

    /// Save a named read-only SQL query. After saving, the query is
    /// exposed as two MCP resources — see the struct-level docs on
    /// [`SaveQueryParams`] for the full URI pattern.
    #[tool(
        description = "Save a named read-only SQL query. Creates two resources: `hyper://queries/{name}/definition` (sql + metadata JSON) and `hyper://queries/{name}/result` (re-runs the SQL on every read). Persisted in the workspace when `--workspace` is set; session-only otherwise. Rejects non-read-only SQL and duplicate names; delete first to overwrite."
    )]
    fn save_query(
        &self,
        Parameters(params): Parameters<SaveQueryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("save_query") {
            return Self::err_content(e);
        }
        // Enforce read-only SQL at save time. This is belt-and-braces: the
        // result resource runs via `execute_query_to_json` which would
        // reject DDL/DML anyway, but rejecting here produces a clearer
        // error and prevents the row landing in the meta-table at all.
        if !is_read_only_sql(&params.sql) {
            return Self::err_content(McpError::new(
                ErrorCode::SqlError,
                "save_query only accepts read-only SQL (SELECT / WITH / EXPLAIN / SHOW / VALUES). \
                 Use the execute tool for DDL/DML, not save_query.",
            ));
        }
        if params.name.is_empty() {
            return Self::err_content(McpError::new(
                ErrorCode::SchemaMismatch,
                "Saved query name must not be empty.",
            ));
        }
        let query = SavedQuery {
            name: params.name.clone(),
            sql: params.sql,
            description: params.description,
            created_at: chrono::Utc::now(),
        };
        let store = Arc::clone(&self.saved_queries);
        let result = self.with_saved_query_store(|engine| store.save(engine, query.clone()));
        match result {
            Ok(()) => {
                // Both resources for this query name are new — nudge
                // clients to refresh their catalog so they see the new
                // `hyper://queries/{name}/...` entries.
                self.notify_resource_list_changed();
                Self::ok_content(json!({
                    "saved": true,
                    "name": query.name,
                    "resources": [
                        format!("hyper://queries/{}/definition", query.name),
                        format!("hyper://queries/{}/result", query.name),
                    ],
                    "created_at": query.created_at.to_rfc3339(),
                }))
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Delete a named saved query and its two resources.
    #[tool(
        description = "Delete a named saved query. Removes the underlying entry and both `hyper://queries/{name}/...` resources. Returns `{deleted: true}` when the query existed, `{deleted: false}` when it did not (no error)."
    )]
    fn delete_query(
        &self,
        Parameters(params): Parameters<DeleteQueryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("delete_query") {
            return Self::err_content(e);
        }
        let store = Arc::clone(&self.saved_queries);
        let name = params.name.clone();
        let result = self.with_saved_query_store(|engine| store.delete(engine, &name));
        match result {
            Ok(deleted) => {
                if deleted {
                    // Two resources just disappeared — fan out a
                    // list_changed and targeted updates so any subscriber
                    // holding stale `hyper://queries/{name}/...` state
                    // drops it.
                    self.notify_resource_list_changed();
                    self.subscriptions
                        .notify_updated(&format!("hyper://queries/{name}/definition"));
                    self.subscriptions
                        .notify_updated(&format!("hyper://queries/{name}/result"));
                }
                Self::ok_content(json!({
                    "deleted": deleted,
                    "name": params.name,
                }))
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Update prose metadata for a table in the `_table_catalog`.
    #[tool(
        description = "Update prose metadata for a table in the `_table_catalog`: source_url, source_description, purpose, license, notes. Fields you omit stay unchanged; pass an explicit empty string (\"\") to clear a field. Mechanical fields (load_tool, load_params, loaded_at, last_refreshed_at, row_count) are managed by the server. Requires an existing catalog entry — load the table first (load_file / load_data / execute CREATE TABLE) so the stub row is created automatically. Use `database` to target the metadata for a table in a non-primary writable database; read-only attachments are rejected with a clear re-attach-with-writable message. Disabled in read-only mode."
    )]
    fn set_table_metadata(
        &self,
        Parameters(params): Parameters<SetTableMetadataParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("set_table_metadata") {
            return Self::err_content(e);
        }
        let fields = crate::table_catalog::MetadataFields {
            source_url: params.source_url,
            source_description: params.source_description,
            purpose: params.purpose,
            license: params.license,
            notes: params.notes,
        };
        let table_name = params.table.clone();
        let result = self.with_engine(|engine| {
            // Resolve target with require_writable=true so read-only
            // attachments are rejected BEFORE any catalog write
            // (defense-in-depth: ensure_exists_in's CREATE TABLE
            // would also fail at the Hyper layer, but the resolve_db
            // error is more actionable).
            let target_db = self.resolve_db(engine, params.database.as_deref(), None, true)?;
            crate::table_catalog::set_metadata_in(
                engine,
                &table_name,
                &fields,
                target_db.as_deref(),
            )
        });
        match result {
            Ok(entry) => Self::ok_content(entry.to_json()),
            Err(e) => Self::err_content(e),
        }
    }

    /// Returns plugin health, workspace info, table count, total rows, disk
    /// usage, and the list of active directory watchers with their stats.
    #[tool(
        description = "Returns plugin health, workspace info, table count, total rows, disk usage, and active directory watchers."
    )]
    fn status(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(super::engine::Engine::status);

        match result {
            Ok(mut val) => {
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("watchers".into(), self.watchers.to_json());
                    obj.insert("read_only".into(), json!(self.read_only));
                    let attachments: Vec<Value> = self
                        .attachments
                        .list()
                        .iter()
                        .map(super::attach::AttachedDb::to_json)
                        .collect();
                    obj.insert("attachments".into(), Value::Array(attachments));
                }
                Self::ok_content(val)
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// Returns a concise LLM-facing README. Stateless — works
    /// identically in read-only mode. The text itself documents
    /// read-only restrictions, so the tool doesn't branch on
    /// `self.read_only`.
    #[tool(
        description = "Returns a concise LLM-facing README explaining what this MCP does, which tool to use for what, key parameter rules, SQL dialect quirks, and usage examples. Call this once at the start of a session to ground the model in the surface area before issuing other tool calls."
    )]
    #[expect(
        clippy::unused_self,
        reason = "the #[tool] macro dispatches on &self; signature must match the rest of the tool surface even though this tool is stateless"
    )]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "uniform Result<CallToolResult, rmcp::ErrorData> across all tools so the #[tool_router] dispatcher has one signature shape"
    )]
    fn get_readme(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(CallToolResult::success(vec![Content::text(
            crate::readme::README,
        )]))
    }

    /// Attach an additional `.hyper` database under a user-chosen
    /// alias so its tables can participate in cross-database queries.
    #[tool(
        description = "Attach an additional .hyper database under a chosen alias. Tables in the attachment are addressable as `{alias}.public.{table}` in any subsequent SELECT; tables in the primary workspace remain addressable as `local.public.{table}` or by their file stem. Default is read-only; pass writable:true to allow mutations (still respects --read-only). Set on_missing='create' (with writable:true) to create an empty .hyper file at the target path first and then attach it — useful for scratch databases without a separate file-creation step; the parent directory must already exist. Only kind='local_file' is supported today; 'tcp' and 'grpc' (Data 360) are planned. The alias 'local' is reserved for the primary workspace."
    )]
    fn attach_database(
        &self,
        Parameters(params): Parameters<AttachDatabaseParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let writable = params.writable.unwrap_or(false);
        if writable {
            if let Err(e) = self.check_writable("attach_database(writable)") {
                return Self::err_content(e);
            }
        }
        let on_missing = match attach::OnMissing::parse(params.on_missing.as_deref()) {
            Ok(v) => v,
            Err(e) => return Self::err_content(e),
        };
        if on_missing == attach::OnMissing::Create && !writable {
            return Self::err_content(McpError::new(
                ErrorCode::InvalidArgument,
                "on_missing='create' requires writable:true — an empty .hyper file that cannot be written to cannot be populated.",
            ));
        }
        let source = match params.kind.as_str() {
            "local_file" => {
                let Some(raw) = params.path.as_deref() else {
                    return Self::err_content(McpError::new(
                        ErrorCode::InvalidArgument,
                        "kind='local_file' requires a 'path' argument",
                    ));
                };
                let resolved = match on_missing {
                    attach::OnMissing::Error => attach::validate_local_path(raw),
                    attach::OnMissing::Create => attach::validate_local_path_for_create(raw),
                };
                match resolved {
                    Ok(canonical) => AttachSource::LocalFile { path: canonical },
                    Err(e) => return Self::err_content(e),
                }
            }
            other => {
                return Self::err_content(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "Unsupported attach kind '{other}'. Only 'local_file' is supported today; \
                         'tcp' (remote hyperd) and 'grpc' (Data 360) are planned."
                    ),
                ));
            }
        };
        let req = AttachRequest {
            alias: params.alias.clone(),
            source,
            writable,
            on_missing,
        };
        let registry = self.attachments_handle();
        let alias_for_probe = req.alias.clone();
        let result = self.with_engine(|engine| {
            let entry = registry.attach(engine, req.clone())?;
            // Best-effort probe for a table count against the new
            // alias so the LLM sees what just came online without a
            // separate round-trip. Failures here don't invalidate the
            // attach — log and return `null` instead.
            let tables_visible = probe_table_count(engine, &alias_for_probe);
            Ok(json!({
                "alias": entry.alias,
                "kind": entry.source.kind_str(),
                "source": entry.source.to_json(),
                "writable": entry.writable,
                "tables_visible": tables_visible,
            }))
        });
        match result {
            Ok(val) => Self::ok_content(val),
            Err(e) => Self::err_content(e),
        }
    }

    /// Detach a previously attached database.
    #[tool(
        description = "Detach a database previously registered with attach_database. No-op when the alias is unknown. Returns {detached: true/false}."
    )]
    fn detach_database(
        &self,
        Parameters(params): Parameters<DetachDatabaseParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Canonicalize to the registry's stored form. Aliases are
        // lowercased at attach time; watcher `target_db` is also stored
        // canonicalized (via `Engine::resolve_target_db`), so an exact
        // `==` comparison suffices below.
        let alias = params.alias.to_ascii_lowercase();
        // Reject if any active watcher targets this alias. Otherwise the
        // watcher's pool would keep ingesting into the now-detached
        // workspace path; or, if the user re-attached the same alias to
        // a different file, into the wrong database. Fixed by stopping
        // the watcher first via `unwatch_directory`.
        if let Ok(watchers) = self.watchers.watchers.lock() {
            let conflict = watchers
                .values()
                .find(|h| h.target_db.as_deref() == Some(alias.as_str()));
            if let Some(h) = conflict {
                return Self::err_content(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "cannot detach '{alias}': an active watcher on directory '{}' targets it. \
                         Call unwatch_directory(\"{}\") first.",
                        h.directory.display(),
                        h.directory.display()
                    ),
                ));
            }
        }
        let registry = self.attachments_handle();
        let result = self.with_engine(|engine| {
            let outcome = registry.detach(engine, &alias)?;
            if outcome {
                // Drop any cached "_table_catalog exists in this alias"
                // probe so a re-attach to a different file or with
                // different writability won't reuse a stale entry.
                engine.clear_catalog_cache_for(&alias);
            }
            Ok(outcome)
        });
        match result {
            Ok(detached) => {
                Self::ok_content(json!({ "alias": params.alias, "detached": detached }))
            }
            Err(e) => Self::err_content(e),
        }
    }

    /// List currently attached databases.
    ///
    /// Named `list_attached_databases` (not `list_attached`) so it
    /// sits alongside `attach_database` / `detach_database` as a
    /// symmetric verb-database trio. The earlier `list_attached`
    /// name broke the pattern and consistently misled LLM callers
    /// into hallucinating `list_attached_databases` anyway, so the
    /// tool now matches the name the models were already reaching
    /// for.
    #[tool(
        description = "List every database currently attached under an alias: kind, path/endpoint, writable flag, attach time, and (best-effort) a count of visible public-schema tables."
    )]
    fn list_attached_databases(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(|engine| {
            let entries = self.attachments.list();
            let attachments: Vec<Value> = entries
                .iter()
                .map(|entry| {
                    let mut obj = entry.to_json();
                    let tables_visible = probe_table_count(engine, &entry.alias);
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("tables_visible".into(), json!(tables_visible));
                    }
                    obj
                })
                .collect();
            Ok(json!({ "attachments": attachments }))
        });
        match result {
            Ok(val) => Self::ok_content(val),
            Err(e) => Self::err_content(e),
        }
    }

    /// Run a SELECT across local + attached databases and land the
    /// result into a target table. All three modes (`create`,
    /// `append`, `replace`) are explicit — the target's actual
    /// existence must match the chosen mode.
    #[tool(
        description = "Run a SELECT (or WITH / VALUES) across local and attached databases and insert the result into a target table. Required `mode`: 'create' (target must not exist, creates via CREATE TABLE AS), 'append' (target must exist, INSERT INTO ... SELECT), or 'replace' (drops and recreates atomically). `target_database` defaults to the primary workspace ('local' also accepted); any other value must be an attachment registered with writable:true. Optional `temp_attach` attaches additional databases for this call only and detaches them on exit (even on failure). Disabled in read-only mode."
    )]
    fn copy_query(
        &self,
        Parameters(params): Parameters<CopyQueryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("copy_query") {
            return Self::err_content(e);
        }
        let mode = match params.mode.as_str() {
            "create" | "append" | "replace" => params.mode.clone(),
            other => {
                return Self::err_content(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "copy_query mode '{other}' is not supported. Use 'create', 'append', or 'replace'."
                    ),
                ));
            }
        };
        if !is_read_only_sql(&params.sql) {
            return Self::err_content(McpError::new(
                ErrorCode::SqlError,
                "copy_query's `sql` must be a read-only statement (SELECT / WITH / VALUES). \
                 Use the execute tool for raw DDL/DML.",
            ));
        }
        // `target_database = None` and `"local"` both map to the
        // primary workspace (unqualified target name). Anything else
        // must refer to an attached writable database.
        //
        // Canonicalize to the registry's lowercase storage form before
        // both the registry lookup AND the qualified-SQL build path
        // (`perform_copy` → `qualified_name`). Hyper is case-sensitive
        // on quoted identifiers; without canonicalization here, a user
        // attaching as `"My_DB"` (which the registry stores as
        // `"my_db"`) and calling `copy_query(target_database="My_DB")`
        // would fail with "database does not exist" once SQL renders.
        let target_db_owned = params
            .target_database
            .as_deref()
            .filter(|s| !s.eq_ignore_ascii_case(LOCAL_ALIAS))
            .map(str::to_ascii_lowercase);
        let target_db = target_db_owned.as_deref();
        if let Some(alias) = target_db {
            match self.attachments.get(alias) {
                None => {
                    return Self::err_content(McpError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "target_database '{alias}' is not attached. Call attach_database first."
                        ),
                    ));
                }
                Some(entry) if !entry.writable => {
                    return Self::err_content(McpError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "target_database '{alias}' was attached read-only. Re-attach with writable:true to use it as a copy target."
                        ),
                    ));
                }
                Some(_) => {}
            }
        }

        // Pre-validate any temp_attach requests *before* we touch the
        // engine so a bad spec aborts cleanly without a partial attach.
        let temp_specs = params.temp_attach.clone().unwrap_or_default();
        let prepared_temps = match prepare_temp_attachments(&temp_specs, self.is_read_only()) {
            Ok(v) => v,
            Err(e) => return Self::err_content(e),
        };

        let target_table = params.target_table.clone();
        let sql_body = params.sql.clone();
        let load_params = serde_json::to_string(&json!({
            "mode": mode,
            "target_database": params.target_database,
            "target_table": target_table,
            "sql": Self::fmt_sql(&sql_body),
        }))
        .ok();

        let registry = self.attachments_handle();
        let result = self.with_engine(|engine| {
            // Phase 1: install temp attachments.
            let mut temp_aliases: Vec<String> = Vec::new();
            for req in &prepared_temps {
                match registry.attach(engine, req.clone()) {
                    Ok(entry) => temp_aliases.push(entry.alias),
                    Err(e) => {
                        // Roll back attachments installed so far.
                        for alias in &temp_aliases {
                            let _ = registry.detach(engine, alias);
                        }
                        return Err(e);
                    }
                }
            }

            // Phase 2: run the actual copy inside a helper so the
            // cleanup path is unified.
            let copy_outcome = perform_copy(engine, &mode, target_db, &target_table, &sql_body);

            // Phase 3: always detach the temp attachments, even on
            // error — they were installed only for the duration of
            // this call.
            for alias in &temp_aliases {
                if let Err(e) = registry.detach(engine, alias) {
                    tracing::warn!(
                        alias = %alias,
                        err = %e.message,
                        "failed to detach temp attachment after copy_query",
                    );
                }
            }

            // Phase 4: stamp `_table_catalog` inside the same engine
            // borrow the copy just ran under. Kept next to the copy
            // (rather than spun off in a second `with_engine`) so the
            // stub and the data it describes can't diverge — a new
            // engine might not even have the catalog materialized yet.
            // Skipped when the destination is an attached database
            // (their catalog isn't ours) or when the server is bare /
            // read-only. `after_ingest_catalog_update` logs WARN on
            // failure, matching how `load_file` / `load_data` /
            // `execute` register their provenance.
            if copy_outcome.is_ok() && target_db.is_none() {
                let row_count = copy_outcome
                    .as_ref()
                    .ok()
                    .and_then(|v| v.get("row_count").and_then(serde_json::Value::as_i64));
                self.after_ingest_catalog_update(
                    engine,
                    &target_table,
                    "copy_query",
                    load_params.as_deref(),
                    row_count,
                    target_db,
                );
            }

            copy_outcome
        });

        match result {
            Ok(outcome) => {
                // Fan out resource updates so subscribers refresh.
                if target_db.is_none() {
                    self.notify_table_changed(&target_table);
                }
                self.notify_workspace_changed();
                if mode != "append" {
                    // `create` / `replace` add or recreate the table,
                    // which is a resource-list-changing event.
                    self.notify_resource_list_changed();
                }
                Self::ok_content(outcome)
            }
            Err(e) => Self::err_content(e),
        }
    }
}

// --- Prompts ---

#[prompt_router]
impl HyperMcpServer {
    /// Deep analysis of a single table: schema, sample, column statistics, data quality flags.
    #[prompt(
        name = "analyze-table",
        description = "Deep analysis of a single table: schema, sample, column stats, data quality"
    )]
    pub async fn analyze_table(
        &self,
        Parameters(args): Parameters<AnalyzeTableArgs>,
    ) -> Vec<PromptMessage> {
        let context = self.build_analyze_context(&args.table);
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Analyze the `{}` table thoroughly.\n\n{}\n\nPlease:\n\
                    1. Describe each column (what it likely represents based on name and sample values)\n\
                    2. Compute basic statistics using the query tool: min/max/avg for numeric columns, distinct count and top values for text columns\n\
                    3. Flag any data quality issues: unexpected NULLs, suspicious outliers, inconsistent formats\n\
                    4. Summarize your findings in plain English",
                    args.table, context
                ),
            ),
            PromptMessage::new_text(
                PromptMessageRole::Assistant,
                format!(
                    "I'll analyze the `{}` table systematically. Let me start by examining the schema and sample, then run targeted queries for statistics and data quality.",
                    args.table
                ),
            ),
        ]
    }

    /// Compare two tables side-by-side: schema alignment, common keys, JOIN suggestions.
    #[prompt(
        name = "compare-tables",
        description = "Compare two tables: schema alignment, common keys, JOIN opportunities"
    )]
    pub async fn compare_tables(
        &self,
        Parameters(args): Parameters<CompareTablesArgs>,
    ) -> Vec<PromptMessage> {
        let ctx_a = self.build_brief_context(&args.table_a);
        let ctx_b = self.build_brief_context(&args.table_b);
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Compare these two tables:\n\n## Table A: `{}`\n{}\n\n## Table B: `{}`\n{}\n\nPlease:\n\
                    1. Identify columns that appear in both tables (by name or semantic match)\n\
                    2. Suggest likely JOIN keys and the JOIN type (inner, left, etc.)\n\
                    3. Highlight schema differences (column types, nullability)\n\
                    4. Propose 3-5 analytical queries that combine both tables and explain what each reveals",
                    args.table_a, ctx_a, args.table_b, ctx_b
                ),
            ),
            PromptMessage::new_text(
                PromptMessageRole::Assistant,
                format!(
                    "I'll compare `{}` and `{}` systematically — schema alignment first, then join keys, then analytical opportunities.",
                    args.table_a, args.table_b
                ),
            ),
        ]
    }

    /// Systematic data quality assessment: nulls, duplicates, cardinality, outliers.
    #[prompt(
        name = "data-quality",
        description = "Systematic data quality assessment: NULL rates, duplicates, low cardinality, outliers"
    )]
    pub async fn data_quality(
        &self,
        Parameters(args): Parameters<DataQualityArgs>,
    ) -> Vec<PromptMessage> {
        let context = self.build_brief_context(&args.table);
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Run a data quality assessment on the `{}` table.\n\n{}\n\nPlease use the query tool to check:\n\
                    1. NULL rate per column — run SELECT COUNT(*) FILTER (WHERE col IS NULL) / COUNT(*) for each column\n\
                    2. Duplicate rows — compare COUNT(*) vs COUNT(DISTINCT *) or use GROUP BY\n\
                    3. Low-cardinality columns — columns with suspiciously few distinct values\n\
                    4. Numeric outliers — values more than 3 stddev from the mean\n\
                    5. Date sanity — future dates or impossibly old dates in date/timestamp columns\n\n\
                    Summarize findings with severity (critical / warning / info) and suggest remediation for each issue.",
                    args.table, context
                ),
            ),
            PromptMessage::new_text(
                PromptMessageRole::Assistant,
                format!(
                    "I'll perform a systematic data quality assessment on `{}`. Let me run targeted queries for each check category.",
                    args.table
                ),
            ),
        ]
    }

    /// Propose useful analytical queries for a table, optionally guided by a goal.
    #[prompt(
        name = "suggest-queries",
        description = "Suggest analytical SQL queries for a table, optionally guided by a goal"
    )]
    pub async fn suggest_queries(
        &self,
        Parameters(args): Parameters<SuggestQueriesArgs>,
    ) -> Vec<PromptMessage> {
        let context = self.build_analyze_context(&args.table);
        let goal_section = match args.goal.as_deref() {
            Some(g) if !g.is_empty() => format!("\n\nSpecific goal: {g}"),
            _ => String::new(),
        };
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Given the `{}` table:\n\n{}{}\n\nSuggest 5 analytical SQL queries that would be useful for exploring this data. \
                    For each query, provide:\n\
                    - A descriptive title\n\
                    - The exact SQL (valid for Hyper / PostgreSQL-compatible syntax)\n\
                    - One sentence explaining what insight it reveals\n\n\
                    Prefer queries that use aggregations, GROUP BY, window functions, or CTEs to demonstrate the power of SQL analytics.",
                    args.table, context, goal_section
                ),
            ),
            PromptMessage::new_text(
                PromptMessageRole::Assistant,
                format!(
                    "Based on the schema and sample of `{}`, here are 5 analytical queries.",
                    args.table
                ),
            ),
        ]
    }
}

/// The payload of a resource read, carrying both MIME type and serialized
/// content. Different resources speak different formats (JSON for metadata,
/// markdown for human overviews, CSV for spreadsheet consumers), so the
/// resource layer needs to pass both along to the MCP client.
///
/// `Json` variants are pretty-printed when rendered; `Text` variants are
/// emitted verbatim. Tests and prompt helpers can still access the
/// underlying JSON via [`ResourceBody::as_json`] when it's a JSON payload.
#[derive(Debug, Clone)]
pub enum ResourceBody {
    /// Structured JSON — rendered as pretty-printed `application/json`.
    Json(Value),
    /// Free-form text with an explicit MIME type (e.g. `text/markdown`,
    /// `text/csv`).
    Text {
        /// IANA media type, e.g. `text/markdown` or `text/csv`.
        mime_type: String,
        /// The literal text to return to the client, verbatim.
        content: String,
    },
}

impl ResourceBody {
    /// Return the MIME type this body will be served with.
    #[must_use]
    pub fn mime_type(&self) -> &str {
        match self {
            ResourceBody::Json(_) => "application/json",
            ResourceBody::Text { mime_type, .. } => mime_type,
        }
    }

    /// Render the body to the text payload the client will receive.
    /// JSON variants are pretty-printed; text variants return as-is.
    #[must_use]
    pub fn to_text(&self) -> String {
        match self {
            ResourceBody::Json(v) => {
                serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
            }
            ResourceBody::Text { content, .. } => content.clone(),
        }
    }

    /// Borrow the underlying `Value` when this body is JSON. Useful for
    /// tests that want to assert on individual fields without reparsing.
    #[must_use]
    pub fn as_json(&self) -> Option<&Value> {
        match self {
            ResourceBody::Json(v) => Some(v),
            ResourceBody::Text { .. } => None,
        }
    }
}

impl HyperMcpServer {
    /// Produce the body for a resource URI without constructing an MCP
    /// `RequestContext`. Factored out of [`Self::read_resource`] so tests can
    /// exercise URI dispatch without standing up the full MCP runtime.
    ///
    /// Returns `Ok(None)` if the URI isn't recognized at all (the async trait
    /// method surfaces this as an `invalid_params` error to clients).
    ///
    /// The returned [`ResourceBody`] carries its own MIME type so non-JSON
    /// resources (`hyper://readme`, `hyper://tables/{name}/csv-sample`,
    /// etc.) can be served verbatim as markdown / CSV.
    ///
    /// # Errors
    ///
    /// Propagates any [`McpError`] from the underlying engine call
    /// (status probe, table description, CSV sample, saved-query listing,
    /// etc.) and bubbles up [`ErrorCode::TableNotFound`] for
    /// `hyper://tables/{name}/...` URIs whose table is absent from the
    /// workspace.
    pub fn resource_body_for_uri(&self, uri: &str) -> Result<Option<ResourceBody>, McpError> {
        if uri == "hyper://workspace" {
            return self
                .with_engine(super::engine::Engine::status)
                .map(|v| Some(ResourceBody::Json(v)));
        }
        if uri == "hyper://tables" {
            return self
                .with_engine(|engine| {
                    engine
                        .describe_tables()
                        .map(|tables| json!({ "tables": tables }))
                })
                .map(|v| Some(ResourceBody::Json(v)));
        }
        if uri == "hyper://readme" {
            return self.build_readme_body().map(Some);
        }
        if let Some(name) = uri
            .strip_prefix("hyper://tables/")
            .and_then(|rest| rest.strip_suffix("/schema"))
        {
            let name = name.to_string();
            return self
                .with_engine(|engine| {
                    let tables = engine.describe_tables()?;
                    tables
                        .into_iter()
                        .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name.as_str()))
                        .ok_or_else(|| {
                            McpError::new(
                                ErrorCode::TableNotFound,
                                format!("Table '{name}' does not exist"),
                            )
                        })
                })
                .map(|v| Some(ResourceBody::Json(v)));
        }
        if let Some(name) = uri
            .strip_prefix("hyper://tables/")
            .and_then(|rest| rest.strip_suffix("/sample"))
        {
            let name = name.to_string();
            return self
                .with_engine(|engine| engine.sample_table(&name, TABLE_SAMPLE_ROWS))
                .map(|v| Some(ResourceBody::Json(v)));
        }
        if let Some(name) = uri
            .strip_prefix("hyper://tables/")
            .and_then(|rest| rest.strip_suffix("/csv-sample"))
        {
            let name = name.to_string();
            return self.build_csv_sample_body(&name).map(Some);
        }
        if let Some(name) = uri
            .strip_prefix("hyper://queries/")
            .and_then(|rest| rest.strip_suffix("/definition"))
        {
            return self.build_saved_query_definition(name).map(Some);
        }
        if let Some(name) = uri
            .strip_prefix("hyper://queries/")
            .and_then(|rest| rest.strip_suffix("/result"))
        {
            return self.build_saved_query_result(name).map(Some);
        }
        Ok(None)
    }

    /// Build `hyper://queries/{name}/definition`: the stored SQL plus
    /// metadata, as JSON. Returns a `TableNotFound` error when no saved
    /// query has that name.
    fn build_saved_query_definition(&self, name: &str) -> Result<ResourceBody, McpError> {
        let store = Arc::clone(&self.saved_queries);
        let name = name.to_string();
        let query = self.with_saved_query_store(|engine| store.get(engine, &name))?;
        match query {
            Some(q) => Ok(ResourceBody::Json(q.to_json())),
            None => Err(McpError::new(
                ErrorCode::TableNotFound,
                format!("No saved query named '{name}'"),
            )),
        }
    }

    /// Build `hyper://queries/{name}/result`: re-run the stored SQL on
    /// every read and return `{ result: [...], stats: {...} }`. Fresh by
    /// default — there is no cache, and the underlying engine is fast
    /// enough that caching isn't worth the staleness risk.
    fn build_saved_query_result(&self, name: &str) -> Result<ResourceBody, McpError> {
        let store = Arc::clone(&self.saved_queries);
        let name_owned = name.to_string();
        let query = self
            .with_saved_query_store(|engine| store.get(engine, &name_owned))?
            .ok_or_else(|| {
                McpError::new(
                    ErrorCode::TableNotFound,
                    format!("No saved query named '{name_owned}'"),
                )
            })?;
        let sql = query.sql.clone();
        let body = self.with_engine(|engine| {
            let timer = crate::stats::StatsTimer::start();
            let rows = engine.execute_query_to_json(&sql)?;
            let elapsed = timer.elapsed_ms();
            let result_size = serde_json::to_string(&rows).map_or(0, |s| s.len() as u64);
            let stats = crate::stats::QueryStats {
                operation: "saved_query".into(),
                rows_returned: rows.len() as u64,
                rows_scanned: 0,
                elapsed_ms: elapsed,
                result_size_bytes: result_size,
                tables_touched: vec![],
            };
            Ok(json!({
                "name": query.name,
                "sql": Self::fmt_sql(&query.sql),
                "result": rows,
                "stats": stats.to_json(),
            }))
        })?;
        Ok(ResourceBody::Json(body))
    }

    /// Produce the list of MCP resources without constructing an MCP
    /// `RequestContext`. Factored out of [`Self::list_resources`] for tests.
    ///
    /// Returns one URI for the workspace, one for the full tables list, one
    /// for the workspace readme, three per existing table (schema, sample,
    /// csv-sample), and two per saved query (definition, result).
    #[must_use]
    pub fn list_resource_uris(&self) -> Vec<String> {
        let mut uris = vec![
            "hyper://workspace".to_string(),
            "hyper://tables".to_string(),
            "hyper://readme".to_string(),
        ];
        if let Ok(tables) = self.with_engine(super::engine::Engine::describe_tables) {
            // `describe_tables` already filters out `_hyperdb_*` meta-
            // tables via `is_internal_table`, so any table we see here
            // is user-visible.
            for table in tables {
                if let Some(name) = table.get("name").and_then(|v| v.as_str()) {
                    uris.push(format!("hyper://tables/{name}/schema"));
                    uris.push(format!("hyper://tables/{name}/sample"));
                    uris.push(format!("hyper://tables/{name}/csv-sample"));
                }
            }
        }
        let store = Arc::clone(&self.saved_queries);
        if let Ok(saved) = self.with_saved_query_store(|engine| store.list(engine)) {
            for q in saved {
                uris.push(format!("hyper://queries/{}/definition", q.name));
                uris.push(format!("hyper://queries/{}/result", q.name));
            }
        }
        uris
    }

    /// Build the `hyper://readme` markdown body: a human-friendly overview
    /// of the current workspace, its tables, and pointers to the other
    /// resources and tools an LLM might reach for.
    ///
    /// Designed to be dropped into an LLM context block so the model can
    /// orient itself in a single resource read without first calling
    /// `status` and `describe` tools.
    fn build_readme_body(&self) -> Result<ResourceBody, McpError> {
        let status = self.with_engine(super::engine::Engine::status)?;
        let tables = self
            .with_engine(super::engine::Engine::describe_tables)
            .unwrap_or_default();

        let workspace_mode = status
            .get("workspace_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let workspace_path = status
            .get("workspace_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let read_only = status
            .get("read_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let table_count = tables.len();

        let mut md = String::new();
        md.push_str("# HyperDB workspace\n\n");
        let _ = writeln!(
            md,
            "- Mode: **{workspace_mode}**{}\n",
            if read_only { " (read-only)" } else { "" }
        );
        if !workspace_path.is_empty() {
            let _ = writeln!(md, "- Path: `{workspace_path}`\n");
        }
        let _ = write!(md, "- Tables: **{table_count}**\n\n");

        if tables.is_empty() {
            md.push_str(
                "_No tables loaded yet._ Use the `load_file` or `load_data` tools to \
                 ingest CSV / JSON / Parquet / Arrow IPC data; call `inspect_file` \
                 first if you're unsure of the schema.\n",
            );
        } else {
            md.push_str("## Tables\n\n");
            md.push_str("| Table | Rows | Columns |\n");
            md.push_str("|---|---:|---|\n");
            for t in &tables {
                let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let rows = t
                    .get("row_count")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let cols: Vec<String> = t
                    .get("columns")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|c| {
                                let n = c.get("name")?.as_str()?;
                                let ty = c.get("type")?.as_str()?;
                                Some(format!("`{n}` {ty}"))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let _ = writeln!(md, "| `{name}` | {rows} | {} |\n", cols.join(", "));
            }
            md.push('\n');
            md.push_str("## Related resources\n\n");
            for t in &tables {
                if let Some(name) = t.get("name").and_then(|v| v.as_str()) {
                    let _ = write!(md, "- `hyper://tables/{name}/schema` — JSON schema and row count\n\
                         - `hyper://tables/{name}/sample` — first {TABLE_SAMPLE_ROWS} rows as JSON\n\
                         - `hyper://tables/{name}/csv-sample` — first {TABLE_CSV_SAMPLE_ROWS} rows as CSV\n");
                }
            }
            md.push('\n');
        }

        md.push_str(
            "## Tool hints\n\n\
             - `query(sql)` — read-only SQL (SELECT / WITH / EXPLAIN / SHOW / VALUES).\n\
             - `execute(sql)` — DDL/DML (disabled in read-only mode).\n\
             - `sample(table, n)` — configurable row sample; the fixed-size\n  \
               `hyper://tables/{name}/sample` resource uses n=5.\n\
             - `inspect_file(path)` — dry-run schema inference before loading.\n\
             - `chart(sql, chart_type, ...)` — render a PNG/SVG from a query.\n\
             - `export(sql|table, path, format)` — write to CSV / Parquet / Arrow IPC / .hyper.\n",
        );

        Ok(ResourceBody::Text {
            mime_type: "text/markdown".into(),
            content: md,
        })
    }

    /// Build the `hyper://tables/{name}/csv-sample` body: first
    /// [`TABLE_CSV_SAMPLE_ROWS`] rows of a table as `text/csv`, with a
    /// header row derived from the sample schema.
    fn build_csv_sample_body(&self, table: &str) -> Result<ResourceBody, McpError> {
        let sample =
            self.with_engine(|engine| engine.sample_table(table, TABLE_CSV_SAMPLE_ROWS))?;

        // Columns come from the sample's `schema` field in the order Hyper
        // reports them; fall back to keys of the first row if that's empty
        // (can happen transiently during catalog desync).
        let header: Vec<String> = sample
            .get("schema")
            .and_then(|v| v.as_array())
            .map(|cols| {
                cols.iter()
                    .filter_map(|c| c.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty())
            .or_else(|| {
                sample
                    .get("rows")
                    .and_then(|v| v.as_array())
                    .and_then(|rows| rows.first())
                    .and_then(|r| r.as_object())
                    .map(|o| o.keys().cloned().collect())
            })
            .unwrap_or_default();

        let mut wtr = csv::Writer::from_writer(Vec::<u8>::new());
        if !header.is_empty() {
            wtr.write_record(&header).map_err(|e| {
                McpError::new(
                    ErrorCode::InternalError,
                    format!("Failed to write CSV header: {e}"),
                )
            })?;
        }
        if let Some(rows) = sample.get("rows").and_then(|v| v.as_array()) {
            for row in rows {
                let record: Vec<String> = header
                    .iter()
                    .map(|col| row.get(col).map(value_to_csv_cell).unwrap_or_default())
                    .collect();
                wtr.write_record(&record).map_err(|e| {
                    McpError::new(
                        ErrorCode::InternalError,
                        format!("Failed to write CSV row: {e}"),
                    )
                })?;
            }
        }
        let bytes = wtr.into_inner().map_err(|e| {
            McpError::new(
                ErrorCode::InternalError,
                format!("Failed to finalize CSV: {e}"),
            )
        })?;
        let content = String::from_utf8(bytes).map_err(|e| {
            McpError::new(
                ErrorCode::InternalError,
                format!("CSV produced invalid UTF-8: {e}"),
            )
        })?;

        Ok(ResourceBody::Text {
            mime_type: "text/csv".into(),
            content,
        })
    }

    /// Build a full analysis context block: schema, row count, and a 10-row sample.
    /// Returns a markdown-formatted string ready to embed in a prompt message.
    fn build_analyze_context(&self, table: &str) -> String {
        match self.with_engine(|engine| engine.sample_table(table, 10)) {
            Ok(sample) => format!(
                "Schema and sample:\n```json\n{}\n```",
                serde_json::to_string_pretty(&sample).unwrap_or_else(|_| sample.to_string())
            ),
            Err(e) => format!("(Could not load table context: {e})"),
        }
    }

    /// Build a brief context block: schema and row count only, no rows.
    fn build_brief_context(&self, table: &str) -> String {
        match self.with_engine(|engine| engine.sample_table(table, 5)) {
            Ok(sample) => format!(
                "```json\n{}\n```",
                serde_json::to_string_pretty(&sample).unwrap_or_else(|_| sample.to_string())
            ),
            Err(e) => format!("(Could not load table context: {e})"),
        }
    }
}

// --- ServerHandler: tools, prompts, and resources ---

#[tool_handler]
#[prompt_handler]
impl ServerHandler for HyperMcpServer {
    fn get_info(&self) -> ServerInfo {
        let sql_dialect = "\n\
\n\
SQL DIALECT — Salesforce Data Cloud SQL (PostgreSQL-compatible with extensions).\n\
Key differences from standard PostgreSQL an LLM should know:\n\
\n\
TYPES\n\
- Supported: SMALLINT, INTEGER/INT, BIGINT, REAL/FLOAT4, DOUBLE PRECISION/FLOAT8,\n\
  NUMERIC(p,s)/DECIMAL(p,s), BOOLEAN, TEXT, CHAR(n), VARCHAR(n), BYTES,\n\
  DATE, TIME, TIMESTAMP, TIMESTAMPTZ, INTERVAL, and arrays of any atomic type\n\
- NUMERIC precision > 18 requires .hyper file format version 3 (default in this MCP)\n\
- No SERIAL / BIGSERIAL / UUID / JSON / JSONB / geometry types\n\
\n\
SELECT / QUERY\n\
- LIMIT / OFFSET work as in PostgreSQL; TOP N is also accepted\n\
- LATERAL is optional: subqueries in FROM always see preceding FROM items implicitly\n\
- DISTINCT ON (expr, ...) is supported\n\
- FROM clause is optional (can evaluate expressions without a table)\n\
- Function calls may appear directly in the FROM list\n\
- information_schema and pg_catalog do NOT exist; use the describe/sample tools\n\
\n\
GROUP BY / AGGREGATION\n\
- GROUPING SETS, ROLLUP, CUBE all supported\n\
- GROUP BY DISTINCT removes duplicate grouping sets before processing\n\
- FILTER (WHERE ...) clause supported on aggregate calls\n\
- Ordered-set aggregates: MODE(), PERCENTILE_CONT(), PERCENTILE_DISC() with WITHIN GROUP (ORDER BY ...)\n\
- APPROX_COUNT_DISTINCT() for fast approximate cardinality\n\
- GROUPING() function identifies which columns are aggregated in GROUPING SETS\n\
\n\
WINDOW FUNCTIONS\n\
- Standard: row_number, rank, dense_rank, percent_rank, cume_dist, ntile, lag, lead,\n\
  first_value, last_value, nth_value\n\
- Hyper extension: modified_rank() — like rank() but assigns the LOWEST rank on ties\n\
- IGNORE NULLS / RESPECT NULLS supported on last_value only\n\
- nth_value supports FROM FIRST / FROM LAST\n\
- Frame modes: ROWS, RANGE, GROUPS; EXCLUDE CURRENT ROW / GROUP / TIES / NO OTHERS\n\
- Window-specific functions do NOT support DISTINCT or ORDER BY in their argument list\n\
\n\
SET-RETURNING FUNCTIONS (usable in FROM)\n\
- unnest(array) — expands an array to rows; supports WITH ORDINALITY\n\
- generate_series(start, stop [, step]) — numeric and datetime variants\n\
- external(path, format => '...') — reads Parquet, CSV, Iceberg etc. directly from files\n\
\n\
SET OPERATORS\n\
- UNION, INTERSECT, EXCEPT all supported; INTERSECT binds tighter than UNION/EXCEPT\n\
- ORDER BY and LIMIT/OFFSET can appear on parenthesized sub-expressions or the final result\n\
\n\
CTEs\n\
- WITH and WITH RECURSIVE both supported\n\
- CTEs evaluate once per query execution even if referenced multiple times\n\
\n\
IDENTIFIERS\n\
- Unquoted identifiers are folded to lowercase; double-quote to preserve case or use special chars\n\
- Quote names containing uppercase letters, digits at the start, or special characters\n\
\n\
NOT AVAILABLE IN HYPER (Data 360 / Data Cloud-only features)\n\
- AI functions: AI_CLASSIFY, AI_SENTIMENT, and other Data Cloud AI scalar functions\n\
- Data Cloud federation / streaming-specific functions\n\
\n\
Full SQL reference: https://developer.salesforce.com/docs/data/data-cloud-query-guide/references/dc-sql-reference";

        let header = if self.read_only {
            "HyperDB MCP (read-only): SQL analytics for LLM workflows. Query existing tables, \
             sample data, export results. Mutating operations are disabled. \
             Call get_readme for a concise tool index, parameter rules, and usage examples."
        } else {
            "HyperDB MCP: instant SQL analytics for LLM workflows. Load data (CSV, JSON, Parquet, \
             Arrow IPC, Apache Iceberg), query with SQL, export results (Parquet, Iceberg, Arrow IPC, \
             CSV, Hyper). Use query for SELECT and execute for DDL/DML. \
             Call get_readme for a concise tool index, parameter rules, and usage examples."
        };
        let instructions = format!("{header}{sql_dialect}");
        let mut server_info = Implementation::default();
        server_info.name = "HyperDB".into();
        server_info.title = Some("HyperDB — Hyper SQL Analytics".into());
        server_info.version = env!("CARGO_PKG_VERSION").into();
        server_info.description = Some(
            "MCP server for Tableau Hyper: instant SQL analytics over \
             CSV, JSON, Parquet, Arrow IPC, and Apache Iceberg with schema inference, \
             partial schema overrides, full-file numeric widening, and \
             dry-run file inspection. SQL dialect is PostgreSQL-compatible with \
             extensions (Salesforce Data Cloud SQL). Full SQL reference: \
             https://developer.salesforce.com/docs/data/data-cloud-query-guide/references/dc-sql-reference/data-cloud-sql-context.html"
                .into(),
        );

        let mut info = ServerInfo::default();
        info.instructions = Some(instructions);
        info.server_info = server_info;
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            // Resource subscriptions + list-changed notifications: lets
            // clients subscribe to any `hyper://...` URI and receive a
            // notification whenever the underlying data has moved,
            // without polling.
            .enable_resources_subscribe()
            .enable_resources_list_changed()
            .build();
        info
    }

    /// Handle a `resources/subscribe` request by recording the calling
    /// peer in the registry under the requested URI.
    ///
    /// MCP does not mandate that the server validate the URI exists
    /// beforehand — subscriptions to URIs that don't resolve today (e.g.
    /// a saved-query result before `save_query` is called) are allowed
    /// and will start delivering notifications as soon as the URI
    /// becomes reachable.
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        self.subscriptions.subscribe(&request.uri, context.peer);
        Ok(())
    }

    /// Handle a `resources/unsubscribe` request. Clears every subscription
    /// recorded against the URI in this process (see the module-level
    /// docs on [`crate::subscriptions`] for why we don't attempt to match
    /// peers individually).
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        self.subscriptions.unsubscribe(&request.uri, &context.peer);
        Ok(())
    }

    /// List MCP resources: the workspace, the tables list, a markdown
    /// readme, and three entries per existing table (schema, JSON sample,
    /// CSV sample). Calling this lazily starts the engine, so it doubles
    /// as a "wake up" signal for MCP clients that pre-fetch resources at
    /// connection time.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        let mut resources = vec![
            RawResource {
                uri: "hyper://workspace".into(),
                name: "Workspace Info".into(),
                title: Some("Hyper Workspace".into()),
                description: Some("Workspace mode, table count, total rows, disk usage".into()),
                mime_type: Some("application/json".into()),
                size: None,
                icons: None,
                meta: None,
            }
            .no_annotation(),
            RawResource {
                uri: "hyper://tables".into(),
                name: "All Tables".into(),
                title: Some("All Tables".into()),
                description: Some("List of all tables with column schemas and row counts".into()),
                mime_type: Some("application/json".into()),
                size: None,
                icons: None,
                meta: None,
            }
            .no_annotation(),
            RawResource {
                uri: "hyper://readme".into(),
                name: "Workspace Readme".into(),
                title: Some("HyperDB workspace readme".into()),
                description: Some(
                    "Markdown overview of the workspace: tables, row counts, related \
                     resources, and tool hints for LLMs orienting themselves."
                        .into(),
                ),
                mime_type: Some("text/markdown".into()),
                size: None,
                icons: None,
                meta: None,
            }
            .no_annotation(),
        ];

        if let Ok(tables) = self.with_engine(super::engine::Engine::describe_tables) {
            // `describe_tables` already excludes `_hyperdb_*` meta-
            // tables (see `is_internal_table`), so the resource
            // catalog only surfaces user-visible tables.
            for table in tables {
                if let Some(name) = table.get("name").and_then(|v| v.as_str()) {
                    let row_count = table
                        .get("row_count")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    resources.push(
                        RawResource {
                            uri: format!("hyper://tables/{name}/schema"),
                            name: format!("Schema of {name}"),
                            title: Some(format!("{name} schema")),
                            description: Some(format!(
                                "Column schema and row count ({row_count} rows) for table '{name}'"
                            )),
                            mime_type: Some("application/json".into()),
                            size: None,
                            icons: None,
                            meta: None,
                        }
                        .no_annotation(),
                    );
                    resources.push(
                        RawResource {
                            uri: format!("hyper://tables/{name}/sample"),
                            name: format!("Sample of {name}"),
                            title: Some(format!("{name} sample (JSON)")),
                            description: Some(format!(
                                "First {TABLE_SAMPLE_ROWS} rows of '{name}' as JSON, with schema"
                            )),
                            mime_type: Some("application/json".into()),
                            size: None,
                            icons: None,
                            meta: None,
                        }
                        .no_annotation(),
                    );
                    resources.push(
                        RawResource {
                            uri: format!("hyper://tables/{name}/csv-sample"),
                            name: format!("CSV sample of {name}"),
                            title: Some(format!("{name} sample (CSV)")),
                            description: Some(format!(
                                "First {TABLE_CSV_SAMPLE_ROWS} rows of '{name}' as CSV"
                            )),
                            mime_type: Some("text/csv".into()),
                            size: None,
                            icons: None,
                            meta: None,
                        }
                        .no_annotation(),
                    );
                }
            }
        }

        let store = Arc::clone(&self.saved_queries);
        if let Ok(saved) = self.with_saved_query_store(|engine| store.list(engine)) {
            for q in saved {
                let desc = q
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Saved read-only SQL query '{}'", q.name));
                resources.push(
                    RawResource {
                        uri: format!("hyper://queries/{}/definition", q.name),
                        name: format!("Query: {}", q.name),
                        title: Some(format!("{} (definition)", q.name)),
                        description: Some(format!("SQL + metadata for saved query '{}'", q.name)),
                        mime_type: Some("application/json".into()),
                        size: None,
                        icons: None,
                        meta: None,
                    }
                    .no_annotation(),
                );
                resources.push(
                    RawResource {
                        uri: format!("hyper://queries/{}/result", q.name),
                        name: format!("Result: {}", q.name),
                        title: Some(format!("{} (result)", q.name)),
                        description: Some(format!("{desc} — re-runs on every read")),
                        mime_type: Some("application/json".into()),
                        size: None,
                        icons: None,
                        meta: None,
                    }
                    .no_annotation(),
                );
            }
        }

        Ok(ListResourcesResult {
            resources,
            next_cursor: None,
            meta: None,
        })
    }

    /// Advertise URI templates so clients can construct resource URIs for
    /// tables they know about without round-tripping `list_resources`.
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, rmcp::ErrorData> {
        let templates = vec![
            RawResourceTemplate {
                uri_template: "hyper://tables/{name}/schema".into(),
                name: "Table Schema".into(),
                title: Some("Table Schema".into()),
                description: Some(
                    "Column schema, types, nullability, and row count for a named table".into(),
                ),
                mime_type: Some("application/json".into()),
                icons: None,
            }
            .no_annotation(),
            RawResourceTemplate {
                uri_template: "hyper://tables/{name}/sample".into(),
                name: "Table Sample (JSON)".into(),
                title: Some("Table Sample".into()),
                description: Some(
                    "First few rows of a named table as JSON, with schema. For a \
                     configurable row count use the `sample` tool instead."
                        .into(),
                ),
                mime_type: Some("application/json".into()),
                icons: None,
            }
            .no_annotation(),
            RawResourceTemplate {
                uri_template: "hyper://tables/{name}/csv-sample".into(),
                name: "Table Sample (CSV)".into(),
                title: Some("Table Sample (CSV)".into()),
                description: Some(
                    "First few rows of a named table as CSV, header-first, for \
                     spreadsheet and Pandas consumers."
                        .into(),
                ),
                mime_type: Some("text/csv".into()),
                icons: None,
            }
            .no_annotation(),
            RawResourceTemplate {
                uri_template: "hyper://queries/{name}/definition".into(),
                name: "Saved Query Definition".into(),
                title: Some("Saved Query Definition".into()),
                description: Some(
                    "Stored SQL plus metadata (description, created_at) for a saved \
                     query registered via the `save_query` tool."
                        .into(),
                ),
                mime_type: Some("application/json".into()),
                icons: None,
            }
            .no_annotation(),
            RawResourceTemplate {
                uri_template: "hyper://queries/{name}/result".into(),
                name: "Saved Query Result".into(),
                title: Some("Saved Query Result".into()),
                description: Some(
                    "Live result of a saved query. The stored SQL re-runs on every \
                     resource read — no caching, always fresh."
                        .into(),
                ),
                mime_type: Some("application/json".into()),
                icons: None,
            }
            .no_annotation(),
        ];
        Ok(ListResourceTemplatesResult {
            resource_templates: templates,
            next_cursor: None,
            meta: None,
        })
    }

    /// Read a resource by URI. Dispatches via
    /// [`HyperMcpServer::resource_body_for_uri`] which returns both the
    /// content and its MIME type (JSON for metadata URIs, markdown for the
    /// workspace readme, CSV for per-table samples).
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, rmcp::ErrorData> {
        let uri = &request.uri;
        let (mime_type, text) = match self.resource_body_for_uri(uri) {
            Ok(Some(body)) => (body.mime_type().to_string(), body.to_text()),
            Ok(None) => {
                return Err(rmcp::ErrorData::invalid_params(
                    format!("Unknown resource URI: {uri}"),
                    None,
                ));
            }
            Err(e) => {
                // Surface errors as JSON so LLMs can parse `code` / `message` /
                // `suggestion` without needing a separate error channel.
                let err_val = serde_json::to_value(&e).unwrap_or(Value::String(e.to_string()));
                let text =
                    serde_json::to_string_pretty(&json!({ "error": err_val })).unwrap_or_default();
                ("application/json".into(), text)
            }
        };

        Ok(ReadResourceResult::new(vec![
            ResourceContents::TextResourceContents {
                uri: uri.clone(),
                mime_type: Some(mime_type),
                text,
                meta: None,
            },
        ]))
    }
}

/// Cheap heuristic: does the given SQL statement create, drop, rename, or
/// otherwise change the shape of the resource catalog? Used by `execute` to
/// decide whether it should fire `notifications/resources/list_changed` in
/// addition to the usual per-URI updates.
///
/// Matches the first keyword (case-insensitive) after whitespace; treats
/// `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE`, `TRUNCATE TABLE`, and
/// `RENAME TABLE` as structural. Plain INSERT / UPDATE / DELETE don't
/// change the table catalog and so don't trigger `list_changed`.
fn is_structural_sql(sql: &str) -> bool {
    let trimmed = sql.trim_start();
    let first: String = trimmed
        .chars()
        .take_while(|c| c.is_alphabetic())
        .flat_map(char::to_uppercase)
        .collect();
    matches!(
        first.as_str(),
        "CREATE" | "DROP" | "ALTER" | "TRUNCATE" | "RENAME"
    )
}

/// Render a JSON cell value into a CSV string. Scalars are emitted in their
/// natural form (numbers as `to_string`, booleans as `true` / `false`,
/// strings verbatim); objects and arrays are re-encoded as compact JSON so
/// the CSV round-trips through re-parsing if needed. `null` becomes the
/// empty string, matching typical spreadsheet conventions.
fn value_to_csv_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

/// Heuristic format detection for inline data: if it starts with `[` or `{`
/// it's JSON, otherwise CSV. Used when the caller omits the `format` parameter.
fn detect_format(data: &str) -> String {
    let trimmed = data.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        "json".into()
    } else {
        "csv".into()
    }
}

/// Generate a nanosecond-based suffix to make temp table names unique within
/// a session. Not cryptographically random — collisions are astronomically
/// unlikely for sequential tool calls.
fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", t.as_nanos() % 1_000_000_000)
}

/// Build a fully-qualified `"db"."schema"."table"` name. `db` is the
/// target alias; `None` means "the primary workspace", which resolves
/// via [`Engine::primary_db_name`]. The `public` schema is assumed
/// because every tool in this crate materializes into `public`.
///
/// Note: while `AttachRegistry` now pins `schema_search_path` to the
/// primary on every attach (so unqualified local writes succeed too),
/// the `copy_query` path still fully-qualifies the target so that
/// switching the target to an attached alias requires no SQL
/// rewriting — one code path covers local and remote targets.
fn qualified_name(engine: &Engine, db: Option<&str>, table: &str) -> String {
    let alias = db.map_or_else(|| engine.primary_db_name(), str::to_string);
    let escaped_alias = alias.replace('"', "\"\"");
    let escaped_table = table.replace('"', "\"\"");
    format!("\"{escaped_alias}\".\"public\".\"{escaped_table}\"")
}

/// `true` if the target resolves to an existing relation, `false` if
/// Hyper reports it as missing, `Err` on any other failure. Uses a
/// `LIMIT 0` probe rather than a catalog lookup because attached
/// databases aren't surfaced by [`Engine::describe_tables`].
fn target_exists(engine: &Engine, db: Option<&str>, table: &str) -> Result<bool, McpError> {
    let sql = format!(
        "SELECT 1 FROM {} LIMIT 0",
        qualified_name(engine, db, table)
    );
    match engine.execute_query_to_json(&sql) {
        Ok(_) => Ok(true),
        Err(e) => {
            let m = e.message.to_lowercase();
            let missing = m.contains("does not exist")
                || m.contains("undefined table")
                || e.message.contains("42P01");
            if missing {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// Fetch `COUNT(*)` against the fully-qualified target. Returns 0 if
/// the query fails (e.g. after a catalog-invalidation quirk) so the
/// tool still returns a result — the caller cares that the copy
/// succeeded, not about bookkeeping fidelity.
fn count_rows(engine: &Engine, db: Option<&str>, table: &str) -> i64 {
    let sql = format!(
        "SELECT COUNT(*) AS cnt FROM {}",
        qualified_name(engine, db, table)
    );
    engine
        .execute_query_to_json(&sql)
        .ok()
        .and_then(|rows| {
            rows.first()
                .and_then(|r| r.get("cnt").and_then(serde_json::Value::as_i64))
        })
        .unwrap_or(0)
}

/// Best-effort probe for public-schema tables visible under an alias.
/// Returns `Value::Null` on any error so the LLM sees "not available"
/// rather than a fabricated zero.
fn probe_table_count(engine: &Engine, alias: &str) -> Value {
    let escaped_alias = alias.replace('"', "\"\"");
    let sql = format!(
        "SELECT COUNT(*) AS cnt FROM \"{escaped_alias}\".pg_catalog.pg_tables WHERE schemaname = 'public'"
    );
    match engine.execute_query_to_json(&sql) {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.get("cnt").and_then(serde_json::Value::as_i64))
            .map_or(Value::Null, |n| json!(n)),
        Err(_) => Value::Null,
    }
}

/// Validate and convert `copy_query`'s `temp_attach` specs into
/// [`AttachRequest`]s. Runs entirely up front (no engine touching)
/// so a bad alias or path aborts cleanly before any ATTACH is issued.
fn prepare_temp_attachments(
    specs: &[AttachSpec],
    read_only: bool,
) -> Result<Vec<AttachRequest>, McpError> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let writable = spec.writable.unwrap_or(false);
        if writable && read_only {
            return Err(McpError::new(
                ErrorCode::ReadOnlyViolation,
                format!(
                    "temp_attach for alias '{}' requested writable:true but the server is --read-only",
                    spec.alias
                ),
            ));
        }
        let on_missing = attach::OnMissing::parse(spec.on_missing.as_deref())?;
        if on_missing == attach::OnMissing::Create && !writable {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "temp_attach alias '{}' has on_missing='create' but writable is not true — \
                     an empty .hyper file that cannot be written to cannot be populated.",
                    spec.alias
                ),
            ));
        }
        let source = match spec.kind.as_str() {
            "local_file" => {
                let Some(raw) = spec.path.as_deref() else {
                    return Err(McpError::new(
                        ErrorCode::InvalidArgument,
                        format!("temp_attach alias '{}' requires a 'path'", spec.alias),
                    ));
                };
                let resolved = match on_missing {
                    attach::OnMissing::Error => attach::validate_local_path(raw)?,
                    attach::OnMissing::Create => attach::validate_local_path_for_create(raw)?,
                };
                AttachSource::LocalFile { path: resolved }
            }
            other => {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "Unsupported temp_attach kind '{other}' for alias '{}'. Only 'local_file' is supported today.",
                        spec.alias
                    ),
                ));
            }
        };
        attach::validate_alias(&spec.alias)?;
        out.push(AttachRequest {
            alias: spec.alias.clone(),
            source,
            writable,
            on_missing,
        });
    }
    Ok(out)
}

/// Execute the chosen copy mode against the fully-qualified target
/// and return a JSON summary. Extracted from the `copy_query` handler
/// so the caller can run it inside the temp-attach cleanup wrapper
/// without re-duplicating the match arms.
fn perform_copy(
    engine: &Engine,
    mode: &str,
    target_db: Option<&str>,
    target_table: &str,
    sql_body: &str,
) -> Result<Value, McpError> {
    let qualified = qualified_name(engine, target_db, target_table);
    let exists = target_exists(engine, target_db, target_table)?;
    let timer = crate::stats::StatsTimer::start();

    match mode {
        "create" => {
            if exists {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "Target '{target_table}' already exists. Use mode='append' to add rows or mode='replace' to drop and recreate."
                    ),
                ));
            }
            engine.execute_command(&format!("CREATE TABLE {qualified} AS {sql_body}"))?;
        }
        "append" => {
            if !exists {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "Target '{target_table}' does not exist. Use mode='create' to create it from the query or mode='replace' to drop and recreate."
                    ),
                ));
            }
            engine.execute_command(&format!("INSERT INTO {qualified} {sql_body}"))?;
        }
        "replace" => {
            // Hyper auto-commits DDL even inside transactions, so
            // DROP+CREATE isn't atomic across the statement boundary
            // (same caveat documented on `execute_in_transaction`).
            // We still issue them in order — the `IF EXISTS` guard
            // prevents an error when the target is absent, and the
            // follow-up `CREATE TABLE AS` either succeeds or leaves
            // the workspace with a dropped target, which is the
            // expected replace semantics.
            engine.execute_command(&format!("DROP TABLE IF EXISTS {qualified}"))?;
            engine.execute_command(&format!("CREATE TABLE {qualified} AS {sql_body}"))?;
        }
        other => {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                format!("copy_query mode '{other}' is not supported"),
            ));
        }
    }

    let elapsed_ms = timer.elapsed_ms();
    let row_count = count_rows(engine, target_db, target_table);
    Ok(json!({
        "target_table": target_table,
        "target_database": target_db.unwrap_or(LOCAL_ALIAS),
        "mode": mode,
        "row_count": row_count,
        "stats": { "operation": "copy_query", "elapsed_ms": elapsed_ms },
    }))
}
