// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dry-run file inspection for the `inspect_file` MCP tool.
//!
//! Given a path to a CSV, JSON, JSONL, Parquet, or Arrow IPC file, [`inspect_source`]
//! returns the schema we would use to create a table (using the exact same
//! inference + full-file numeric widening as [`crate::ingest`]) along with
//! per-column diagnostics that help an LLM pick a safer schema override
//! *before* running `load_file`:
//!
//! * `null_count` — how many null / empty cells were seen in the full file.
//! * `min` / `max` — smallest and largest values observed for numeric columns.
//! * `sample_values` — a handful of raw string values from the first rows.
//!
//! Nothing is written to Hyper; the engine is not even touched. The result is
//! intentionally cheap enough to call eagerly before every `load_file`, which
//! is the workflow we recommend in the tool description and error suggestions.
//!
//! For non-CSV inputs we fall back to the file's embedded schema (Parquet /
//! Arrow IPC carry exact types) and skip min/max because extracting those
//! cheaply would mean decoding every batch.

use crate::error::{ErrorCode, McpError};
use crate::ingest::{InferredFileFormat, detect_file_format, normalize_json_or_jsonl};
use crate::ingest_arrow::arrow_schema_to_columns;
use crate::schema::{ColumnSchema, infer_csv_schema, infer_json_schema, widen_csv_numeric_columns};
use arrow::datatypes::Schema as ArrowSchema;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

/// Default number of sample rows returned for each column if the caller does
/// not request a specific count. Small enough to keep the response compact for
/// LLM clients while still giving a human-readable preview.
pub const DEFAULT_SAMPLE_ROWS: usize = 5;

/// Per-column diagnostics returned alongside the inferred schema.
#[derive(Debug, Default, Clone)]
pub struct ColumnStats {
    pub null_count: u64,
    /// Minimum observed integer value for numeric columns (as `i128` to cover
    /// anything the widen pass will accept). `None` for non-numeric columns
    /// or columns that only ever contained floats / NULLs.
    pub min_i128: Option<i128>,
    pub max_i128: Option<i128>,
    /// Minimum / maximum observed floating-point value. Populated for `DOUBLE
    /// PRECISION` columns and integer columns that saw a decimal value.
    pub min_f64: Option<f64>,
    pub max_f64: Option<f64>,
    /// Up to `sample_rows` raw string values from the beginning of the file.
    pub sample_values: Vec<String>,
}

/// Full inspection result for a single source.
#[derive(Debug, Clone)]
pub struct InspectReport {
    pub columns: Vec<ColumnSchema>,
    pub stats: Vec<ColumnStats>,
    pub row_count: u64,
    pub file_format: String,
    pub file_size_bytes: u64,
    /// The first `sample_rows` rows as vectors of raw string values, aligned
    /// to `columns`. For non-CSV inputs this is empty.
    pub sample_rows: Vec<Vec<String>>,
}

impl InspectReport {
    /// Serialize into the JSON shape returned by the MCP `inspect_file` tool.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let columns: Vec<Value> = self
            .columns
            .iter()
            .zip(self.stats.iter())
            .map(|(col, s)| {
                let mut obj = json!({
                    "name": col.name,
                    "type": col.hyper_type,
                    "nullable": col.nullable,
                    "null_count": s.null_count,
                    "sample_values": s.sample_values,
                });
                if let Some(m) = s.min_i128 {
                    if let Ok(n) = i64::try_from(m) {
                        obj["min"] = json!(n);
                    } else {
                        obj["min"] = json!(m.to_string());
                    }
                }
                if let Some(m) = s.max_i128 {
                    if let Ok(n) = i64::try_from(m) {
                        obj["max"] = json!(n);
                    } else {
                        obj["max"] = json!(m.to_string());
                    }
                }
                if let Some(m) = s.min_f64 {
                    obj["min_f64"] = json!(m);
                }
                if let Some(m) = s.max_f64 {
                    obj["max_f64"] = json!(m);
                }
                obj
            })
            .collect();
        json!({
            "columns": columns,
            "row_count": self.row_count,
            "file_format": self.file_format,
            "file_size_bytes": self.file_size_bytes,
            "sample_rows": self.sample_rows.iter().map(|row| {
                self.columns.iter().zip(row.iter()).map(|(c, v)| (c.name.clone(), Value::String(v.clone()))).collect::<serde_json::Map<_, _>>()
            }).collect::<Vec<_>>(),
        })
    }
}

/// Top-level dispatcher: pick the right inspector for `path` using the
/// same extension + content-sniffing logic [`crate::ingest::detect_file_format`]
/// drives for the ingest side. That shared dispatcher is what ensures
/// `inspect_file` and `load_file` can never disagree on what a file is —
/// if inspect says "this is JSONL", load will try it as JSONL too.
///
/// # Errors
///
/// - Returns [`ErrorCode::FileNotFound`] if `path` does not exist.
/// - Propagates format-specific errors from the delegated inspector —
///   `inspect_parquet`, `inspect_arrow_ipc`, `inspect_json`, or
///   `inspect_csv` — each of which surfaces its own I/O, decoding,
///   or schema-inference failures.
pub fn inspect_source(path: &str, sample_rows: usize) -> Result<InspectReport, McpError> {
    let file_path = Path::new(path);
    if !file_path.exists() {
        return Err(McpError::new(
            ErrorCode::FileNotFound,
            format!("File not found: {path}"),
        ));
    }
    let file_size = std::fs::metadata(file_path).map_or(0, |m| m.len());
    match detect_file_format(file_path) {
        InferredFileFormat::Parquet => inspect_parquet(path, file_size),
        InferredFileFormat::ArrowIpc => inspect_arrow_ipc(path, file_size),
        InferredFileFormat::Json => inspect_json(path, file_size, sample_rows),
        InferredFileFormat::Csv => inspect_csv(path, file_size, sample_rows),
    }
}

/// JSON / JSONL inspector: reads the file, auto-detects between a
/// top-level array and newline-delimited JSON via
/// [`normalize_json_or_jsonl`], then runs the same
/// [`infer_json_schema`] pass `ingest_json` uses so the reported schema
/// matches what `load_file` would create.
///
/// Per-column stats collected:
///
/// * `null_count` — number of records where the key is missing or its
///   value is `null`.
/// * `sample_values` — up to `sample_rows` stringified non-null values
///   from the first records.
///
/// `min`/`max` are intentionally skipped for JSON: unlike CSV, JSON
/// types are self-describing so the schema already tells you what
/// range to expect, and extracting numeric extrema would require
/// iterating the full dataset in the inspector's hot path.
fn inspect_json(path: &str, file_size: u64, sample_rows: usize) -> Result<InspectReport, McpError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| McpError::new(ErrorCode::FileNotFound, format!("Cannot read file: {e}")))?;
    inspect_json_from_text(&raw, file_size, sample_rows)
}

/// Inspect a JSON / JSONL text string (already in memory) without
/// reading from a file. Used by the `inspect_file` handler when
/// `json_extract_path` has already extracted the relevant slice.
///
/// Public so [`crate::server`] can call it after `extract_json_path`.
///
/// # Errors
///
/// - Propagates errors from [`normalize_json_or_jsonl`] (malformed
///   JSONL) and [`infer_json_schema`] (schema inference failure).
/// - Returns [`ErrorCode::SchemaMismatch`] if the normalized array
///   cannot be re-parsed as JSON for stats collection.
pub fn inspect_json_from_text(
    raw: &str,
    file_size: u64,
    sample_rows: usize,
) -> Result<InspectReport, McpError> {
    // Remember the original shape for the `file_format` field in the
    // report; `normalize_json_or_jsonl` always returns a top-level
    // array so we'd otherwise lose that signal.
    let is_array = raw.trim_start().starts_with('[');
    let file_format = if is_array { "json" } else { "jsonl" };

    let array_text = normalize_json_or_jsonl(raw)?;
    let columns = infer_json_schema(&array_text)?;

    // Parse the array a second time for stats — cheap, and keeps
    // schema inference and stats independent.
    let array: Vec<Value> = serde_json::from_str(&array_text)
        .map_err(|e| McpError::new(ErrorCode::SchemaMismatch, format!("Invalid JSON: {e}")))?;

    let sample_cap = sample_rows.max(1);
    let mut stats: Vec<ColumnStats> = columns.iter().map(|_| ColumnStats::default()).collect();
    let mut sample_rows_out: Vec<Vec<String>> = Vec::new();
    let row_count = array.len() as u64;

    for obj in &array {
        let Some(object) = obj.as_object() else {
            continue;
        };
        if sample_rows_out.len() < sample_cap {
            let mut row_vals = Vec::with_capacity(columns.len());
            for col in &columns {
                row_vals.push(value_preview(object.get(&col.name)));
            }
            sample_rows_out.push(row_vals);
        }
        for (idx, col) in columns.iter().enumerate() {
            let s = &mut stats[idx];
            match object.get(&col.name) {
                None | Some(Value::Null) => s.null_count += 1,
                Some(v) => {
                    if s.sample_values.len() < sample_cap {
                        s.sample_values.push(value_preview(Some(v)));
                    }
                }
            }
        }
    }

    Ok(InspectReport {
        columns,
        stats,
        row_count,
        file_format: file_format.into(),
        file_size_bytes: file_size,
        sample_rows: sample_rows_out,
    })
}

/// Render a JSON value into the short string form used for
/// `sample_values` and `sample_rows` in the inspect report. Scalars
/// stringify naturally; nested arrays/objects are rendered as compact
/// JSON so the preview stays on one line and never grows unboundedly
/// long.
fn value_preview(v: Option<&Value>) -> String {
    const MAX_LEN: usize = 120;
    let raw = match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    };
    // Truncate by characters: slicing at a byte offset panics when it lands
    // inside a multi-byte code point.
    match raw.char_indices().nth(MAX_LEN) {
        Some((cut, _)) => {
            let mut s = raw[..cut].to_string();
            s.push('…');
            s
        }
        None => raw,
    }
}

/// CSV inspector: reuses [`infer_csv_schema`] and [`widen_csv_numeric_columns`]
/// so the schema returned here is *exactly* the one `load_file` would create,
/// then adds a streaming pass that tallies nulls / min / max and captures
/// sample values for each column.
fn inspect_csv(path: &str, file_size: u64, sample_rows: usize) -> Result<InspectReport, McpError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| McpError::new(ErrorCode::FileNotFound, format!("Cannot read file: {e}")))?;

    let mut columns = infer_csv_schema(&text, true)?;
    widen_csv_numeric_columns(text.as_bytes(), true, &mut columns)?;

    let mut stats: Vec<ColumnStats> = columns.iter().map(|_| ColumnStats::default()).collect();
    let mut sample_rows_out: Vec<Vec<String>> = Vec::new();
    let sample_cap = sample_rows.max(1);

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(text.as_bytes());
    let mut row_count: u64 = 0;
    for result in rdr.records() {
        let record = result.map_err(|e| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("CSV parse error at row {}: {e}", row_count + 1),
            )
        })?;
        row_count += 1;
        // Capture sample rows from the top of the file.
        if sample_rows_out.len() < sample_cap {
            sample_rows_out.push(
                record
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
            );
        }
        // Per-column stats.
        for (col_idx, col) in columns.iter().enumerate() {
            let s = &mut stats[col_idx];
            let raw = record.get(col_idx).unwrap_or("");
            let trimmed = raw.trim();
            if trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("null")
                || trimmed.eq_ignore_ascii_case("na")
            {
                s.null_count += 1;
                continue;
            }
            if s.sample_values.len() < sample_cap {
                s.sample_values.push(trimmed.to_string());
            }
            // Integer stats for integer-typed columns (post-widening).
            if matches!(
                col.hyper_type.as_str(),
                "INT" | "INTEGER" | "BIGINT" | "NUMERIC(38,0)"
            ) {
                if let Ok(n) = trimmed.parse::<i128>() {
                    s.min_i128 = Some(s.min_i128.map_or(n, |m| m.min(n)));
                    s.max_i128 = Some(s.max_i128.map_or(n, |m| m.max(n)));
                }
            } else if col.hyper_type == "DOUBLE PRECISION"
                && let Ok(n) = trimmed.parse::<f64>()
            {
                s.min_f64 = Some(s.min_f64.map_or(n, |m| m.min(n)));
                s.max_f64 = Some(s.max_f64.map_or(n, |m| m.max(n)));
            }
        }
    }

    Ok(InspectReport {
        columns,
        stats,
        row_count,
        file_format: "csv".into(),
        file_size_bytes: file_size,
        sample_rows: sample_rows_out,
    })
}

/// Parquet inspector: the file embeds exact types, so the schema is read from
/// Parquet metadata and no widening pass is needed. Min/max extraction would
/// require decoding batches, so we skip it and rely on the fact that Parquet
/// readers already emit correctly-typed values.
fn inspect_parquet(path: &str, file_size: u64) -> Result<InspectReport, McpError> {
    let file = std::fs::File::open(path)
        .map_err(|e| McpError::new(ErrorCode::FileNotFound, format!("Cannot open file: {e}")))?;
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| {
            McpError::new(
                ErrorCode::UnsupportedFormat,
                format!("Invalid Parquet file: {e}"),
            )
        })?;
    let arrow_schema: Arc<ArrowSchema> = Arc::clone(builder.schema());
    // Parquet `num_rows` is an `i64`; a negative value means a corrupt footer.
    // Report 0 rather than panic or silently wrap.
    let row_count = u64::try_from(builder.metadata().file_metadata().num_rows()).unwrap_or(0);
    let columns = arrow_schema_to_columns(&arrow_schema);
    let stats = vec![ColumnStats::default(); columns.len()];
    Ok(InspectReport {
        columns,
        stats,
        row_count,
        file_format: "parquet".into(),
        file_size_bytes: file_size,
        sample_rows: Vec::new(),
    })
}

/// Arrow IPC inspector: schema is read from the file footer.
fn inspect_arrow_ipc(path: &str, file_size: u64) -> Result<InspectReport, McpError> {
    let file = std::fs::File::open(path)
        .map_err(|e| McpError::new(ErrorCode::FileNotFound, format!("Cannot open file: {e}")))?;
    let reader = arrow::ipc::reader::FileReader::try_new(file, None).map_err(|e| {
        McpError::new(
            ErrorCode::UnsupportedFormat,
            format!("Invalid Arrow IPC file: {e}"),
        )
    })?;
    let arrow_schema: Arc<ArrowSchema> = reader.schema();
    let row_count: u64 = reader
        .into_iter()
        .map(|b| b.map_or(0, |rb| rb.num_rows() as u64))
        .sum();
    let columns = arrow_schema_to_columns(&arrow_schema);
    let stats = vec![ColumnStats::default(); columns.len()];
    Ok(InspectReport {
        columns,
        stats,
        row_count,
        file_format: "arrow_ipc".into(),
        file_size_bytes: file_size,
        sample_rows: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_preview_truncates_on_a_char_boundary() {
        // 119 ASCII bytes then a 3-byte char straddling the 120-byte cut.
        let long = format!("{}€€€", "a".repeat(119));
        let preview = value_preview(Some(&Value::String(long)));
        assert!(preview.ends_with('…'));
        assert!(preview.chars().count() <= 121);

        let short = value_preview(Some(&Value::String("héllo".into())));
        assert_eq!(short, "héllo");
    }
}
