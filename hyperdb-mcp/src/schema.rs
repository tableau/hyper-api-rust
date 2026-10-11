// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Schema inference and type mapping.
//!
//! # Three-Tier Inference
//!
//! The tier is selected automatically based on the data source:
//!
//! | Tier | Source | Strategy |
//! |------|--------|----------|
//! | **Exact** | Arrow IPC, Parquet | Types read from file metadata — zero guessing. |
//! | **Structural** | JSON | Full scan of all objects. Per-column type widening (see below). |
//! | **Heuristic** | CSV | Header row for names, first 1 000 rows sampled for types. Ambiguous → TEXT. A second full-file pass promotes INT → BIGINT → NUMERIC(38,0) on out-of-range values and INT/BIGINT → DOUBLE PRECISION on decimals. |
//!
//! All tiers can be bypassed with an explicit `schema` override from the caller.
//!
//! # Type Widening Rules (Structural / Heuristic Tiers)
//!
//! When a JSON or CSV column contains mixed types across rows, these rules
//! determine the final type. Specific rules (implemented in `resolve_type`):
//! - **All null** → TEXT (safe catch-all).
//! - **Uniform non-null** → that type, unchanged.
//! - **Mixed numeric** (Int / `BigInt` / Double) → widest numeric type seen.
//! - **Any other mix** (e.g. Bool + Int, Date + Text) → TEXT.
//!
//! # Arrow / Parquet Type Mapping
//!
//! The exact-tier mapping from Arrow types to Hyper SQL types is handled by
//! `crate::ingest_arrow::arrow_type_to_hyper`. Key mappings:
//!
//! | Arrow | Hyper |
//! |-------|-------|
//! | Int16/Int32/Int64 | SMALLINT/INT/BIGINT |
//! | UInt16/UInt32/UInt64 | INT/BIGINT/BIGINT (promoted to signed) |
//! | Float32/Float64 | DOUBLE PRECISION |
//! | Utf8/LargeUtf8 | TEXT |
//! | Date32/Date64 | DATE |
//! | Timestamp(_, None/Some) | TIMESTAMP / TIMESTAMPTZ |
//! | Decimal128/Decimal256 | NUMERIC |

use crate::error::{ErrorCode, McpError};
use hyperdb_api::{SqlType, TableDefinition};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// A column's name, Hyper SQL type (as a string like `"INT"` or `"DOUBLE PRECISION"`),
/// and nullability. This is the internal schema representation shared across all
/// ingest and table-creation paths.
#[derive(Debug, Clone)]
pub struct ColumnSchema {
    pub name: String,
    /// Hyper type name (e.g. `"TEXT"`, `"BIGINT"`, `"NUMERIC(12,2)"`).
    pub hyper_type: String,
    pub nullable: bool,
}

/// Build a hyperdb-api `TableDefinition` from a list of `ColumnSchema`.
///
/// Uses the consuming builder pattern required by `TableDefinition`.
///
/// # Errors
///
/// - Returns [`ErrorCode::EmptyData`] if `columns` is empty.
/// - Returns [`ErrorCode::SchemaMismatch`] if any column's `hyper_type`
///   cannot be resolved by [`map_hyper_type`].
pub fn build_table_def(
    table_name: &str,
    columns: &[ColumnSchema],
) -> Result<TableDefinition, McpError> {
    if columns.is_empty() {
        return Err(McpError::new(
            ErrorCode::EmptyData,
            "No columns to create table from",
        ));
    }
    let mut def = TableDefinition::new(table_name);
    for col in columns {
        let sql_type = map_hyper_type(&col.hyper_type).ok_or_else(|| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Unknown type: {}", col.hyper_type),
            )
        })?;
        if col.nullable {
            def = def.add_nullable_column(&col.name, sql_type);
        } else {
            def = def.add_required_column(&col.name, sql_type);
        }
    }
    Ok(def)
}

/// Map a user-facing type name string (e.g. `"INT"`, `"NUMERIC(12,2)"`) to a
/// [`hyperdb_api::SqlType`]. Accepts `PostgreSQL` aliases (`INT4`, `FLOAT8`, `VARCHAR`)
/// so schema overrides from diverse sources work without normalization.
#[must_use]
pub fn map_hyper_type(type_name: &str) -> Option<SqlType> {
    let upper = type_name.trim().to_uppercase();
    match upper.as_str() {
        "SMALLINT" | "INT2" => Some(SqlType::small_int()),
        "INT" | "INTEGER" | "INT4" => Some(SqlType::int()),
        "BIGINT" | "INT8" => Some(SqlType::big_int()),
        "FLOAT" | "FLOAT4" | "REAL" => Some(SqlType::double()),
        "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" => Some(SqlType::double()),
        "TEXT" | "VARCHAR" | "STRING" => Some(SqlType::text()),
        "BOOL" | "BOOLEAN" => Some(SqlType::bool()),
        "DATE" => Some(SqlType::date()),
        "TIME" => Some(SqlType::time()),
        "TIMESTAMP" => Some(SqlType::timestamp()),
        "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => Some(SqlType::timestamp_tz()),
        "BYTEA" | "BYTES" => Some(SqlType::bytes()),
        _ if upper.starts_with("NUMERIC") => {
            // Parse NUMERIC(p,s) or default to NUMERIC(38,0)
            if let Some(inner) = upper
                .strip_prefix("NUMERIC(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let parts: Vec<&str> = inner.split(',').collect();
                let precision = parts
                    .first()
                    .and_then(|p| p.trim().parse().ok())
                    .unwrap_or(38);
                let scale = parts
                    .get(1)
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0);
                Some(SqlType::numeric(precision, scale))
            } else {
                Some(SqlType::numeric(38, 0))
            }
        }
        _ => None,
    }
}

// --- Tier 2: JSON Schema Inference ---

/// Intermediate type tag used during schema inference. Mixed Int/`BigInt`/Double
/// widen to the widest seen; any other mix resolves to Text.
#[derive(Debug, Clone, PartialEq)]
enum InferredType {
    Null,
    Bool,
    Int,
    BigInt,
    Double,
    Date,
    Timestamp,
    Text,
}

/// Infer a [`ColumnSchema`] for each key in a JSON array of objects (Tier 2).
///
/// Every object is scanned so that keys appearing in only some rows are detected
/// as nullable. Per-column types are resolved with numeric widening
/// (Int → `BigInt` → Double) and fall back to TEXT for mixed types.
///
/// Uses `BTreeSet`/`BTreeMap` for deterministic column ordering across runs.
///
/// # Errors
///
/// - Returns [`ErrorCode::SchemaMismatch`] if `json_str` is not a valid
///   top-level JSON array of objects.
/// - Returns [`ErrorCode::EmptyData`] if the array is empty.
///
/// # Panics
///
/// Does not panic in practice. The `col_types.get_mut(key).unwrap()` and
/// `col_present.get_mut(key).unwrap()` calls are guarded by a preceding
/// initialization loop that inserts every `key` from `all_keys` into
/// both maps.
pub fn infer_json_schema(json_str: &str) -> Result<Vec<ColumnSchema>, McpError> {
    let array: Vec<serde_json::Map<String, Value>> =
        serde_json::from_str(json_str).map_err(|e| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Invalid JSON array: {e}"),
            )
        })?;

    if array.is_empty() {
        return Err(McpError::new(ErrorCode::EmptyData, "JSON array is empty"));
    }

    // Collect all keys (BTreeSet for deterministic ordering)
    let mut all_keys = BTreeSet::new();
    for obj in &array {
        for key in obj.keys() {
            all_keys.insert(key.clone());
        }
    }

    let total_rows = array.len();
    let mut col_types: BTreeMap<String, Vec<InferredType>> = BTreeMap::new();
    let mut col_present: BTreeMap<String, usize> = BTreeMap::new();

    for key in &all_keys {
        col_types.insert(key.clone(), Vec::new());
        col_present.insert(key.clone(), 0);
    }

    for obj in &array {
        for key in &all_keys {
            match obj.get(key.as_str()) {
                None => {}
                Some(Value::Null) => {
                    col_types.get_mut(key).unwrap().push(InferredType::Null);
                    *col_present.get_mut(key).unwrap() += 1;
                }
                Some(val) => {
                    col_types
                        .get_mut(key)
                        .unwrap()
                        .push(infer_json_value_type(val));
                    *col_present.get_mut(key).unwrap() += 1;
                }
            }
        }
    }

    let mut columns = Vec::new();
    for key in &all_keys {
        let types = &col_types[key];
        let present_count = col_present[key];
        let nullable = present_count < total_rows || types.contains(&InferredType::Null);
        let resolved = resolve_type(types);
        columns.push(ColumnSchema {
            name: key.clone(),
            hyper_type: inferred_to_hyper_name(&resolved),
            nullable,
        });
    }

    Ok(columns)
}

/// Classify a single JSON value. Numbers that fit in i32 are `Int`, larger
/// integers are `BigInt`, anything with a fractional part is `Double`.
/// Strings are further inspected for ISO 8601 date/timestamp patterns.
fn infer_json_value_type(val: &Value) -> InferredType {
    match val {
        Value::Null => InferredType::Null,
        Value::Bool(_) => InferredType::Bool,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i32::try_from(i).is_ok() {
                    InferredType::Int
                } else {
                    InferredType::BigInt
                }
            } else {
                InferredType::Double
            }
        }
        Value::String(s) => infer_string_type(s),
        _ => InferredType::Text,
    }
}

/// Attempt to recognize ISO 8601 date (`YYYY-MM-DD`) or timestamp
/// (`YYYY-MM-DDThh:mm:ss`) patterns in a string value. Returns `Text` for
/// anything that doesn't match.
///
/// # Safety of the string slices
///
/// Both patterns are pure ASCII, so the leftmost clause of each `if` is
/// `s.is_ascii()`. That makes every byte index a char boundary, so the
/// `s[a..b]` slices below cannot panic on multi-byte input; the length
/// checks that follow it guarantee the upper bounds are in range.
fn infer_string_type(s: &str) -> InferredType {
    // Try ISO 8601 date: YYYY-MM-DD
    if s.is_ascii()
        && s.len() == 10
        && s.chars().nth(4) == Some('-')
        && s.chars().nth(7) == Some('-')
        && s[0..4].parse::<u16>().is_ok()
        && s[5..7].parse::<u8>().is_ok()
        && s[8..10].parse::<u8>().is_ok()
    {
        return InferredType::Date;
    }
    // Try ISO 8601 timestamp: YYYY-MM-DDThh:mm:ss
    if s.is_ascii()
        && s.len() >= 19
        && s.chars().nth(10) == Some('T')
        && s[0..10].contains('-')
        && s[11..].contains(':')
    {
        return InferredType::Timestamp;
    }
    InferredType::Text
}

/// Resolve a column's final type from the per-row type observations.
///
/// Rules:
/// 1. All-null columns become TEXT (safe catch-all).
/// 2. Uniform non-null types pass through unchanged.
/// 3. Mixed numeric types widen: Int → `BigInt` → Double.
/// 4. Any other mix (e.g. Bool + Int, Date + Text) collapses to TEXT.
fn resolve_type(types: &[InferredType]) -> InferredType {
    let non_null: Vec<&InferredType> = types.iter().filter(|t| **t != InferredType::Null).collect();
    if non_null.is_empty() {
        return InferredType::Text; // All null → TEXT
    }

    let first = non_null[0];
    let all_same = non_null.iter().all(|t| *t == first);
    if all_same {
        return first.clone();
    }

    // Numeric widening: Int -> BigInt -> Double
    let all_numeric = non_null.iter().all(|t| {
        matches!(
            t,
            InferredType::Int | InferredType::BigInt | InferredType::Double
        )
    });
    if all_numeric {
        if non_null.iter().any(|t| **t == InferredType::Double) {
            return InferredType::Double;
        }
        if non_null.iter().any(|t| **t == InferredType::BigInt) {
            return InferredType::BigInt;
        }
        return InferredType::Int;
    }

    // Mixed types → TEXT
    InferredType::Text
}

/// Convert an [`InferredType`] to the Hyper SQL type name used in DDL.
fn inferred_to_hyper_name(t: &InferredType) -> String {
    match t {
        InferredType::Null | InferredType::Text => "TEXT".into(),
        InferredType::Bool => "BOOL".into(),
        InferredType::Int => "INT".into(),
        InferredType::BigInt => "BIGINT".into(),
        InferredType::Double => "DOUBLE PRECISION".into(),
        InferredType::Date => "DATE".into(),
        InferredType::Timestamp => "TIMESTAMP".into(),
    }
}

// --- Tier 3: CSV Schema Inference ---

/// Infer a [`ColumnSchema`] for each CSV column (Tier 3).
///
/// When `has_header` is true, the first row provides column names; otherwise
/// columns are named `col_0`, `col_1`, etc. Up to 1 000 data rows are sampled
/// to determine types. All CSV columns are marked nullable because CSV has no
/// way to express a NOT NULL constraint.
///
/// # Errors
///
/// - Returns [`ErrorCode::SchemaMismatch`] when the CSV header line or
///   any sampled record cannot be parsed.
/// - Returns [`ErrorCode::EmptyData`] when there are no data rows (to
///   infer column count in the headerless case) or when the header row
///   is empty.
pub fn infer_csv_schema(csv_text: &str, has_header: bool) -> Result<Vec<ColumnSchema>, McpError> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(has_header)
        .from_reader(csv_text.as_bytes());

    let headers: Vec<String> = if has_header {
        reader
            .headers()
            .map_err(|e| {
                McpError::new(ErrorCode::SchemaMismatch, format!("CSV header error: {e}"))
            })?
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    } else {
        // Peek at first record to get column count
        let first = reader.records().next();
        match first {
            Some(Ok(ref rec)) => (0..rec.len()).map(|i| format!("col_{i}")).collect(),
            _ => return Err(McpError::new(ErrorCode::EmptyData, "CSV has no data rows")),
        }
    };

    if headers.is_empty() {
        return Err(McpError::new(ErrorCode::EmptyData, "CSV has no columns"));
    }

    let num_cols = headers.len();
    let mut col_types: Vec<Vec<InferredType>> = vec![Vec::new(); num_cols];

    // Re-read from start for sampling (up to 1000 rows)
    let mut sample_reader = csv::ReaderBuilder::new()
        .has_headers(has_header)
        .from_reader(csv_text.as_bytes());

    for (row_idx, result) in sample_reader.records().enumerate() {
        if row_idx >= 1000 {
            break;
        }
        let record = result.map_err(|e| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("CSV parse error at row {}: {e}", row_idx + 1),
            )
        })?;
        for (col_idx, field) in record.iter().enumerate() {
            if col_idx < num_cols {
                col_types[col_idx].push(infer_csv_field_type(field));
            }
        }
    }

    let columns: Vec<ColumnSchema> = headers
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            let resolved = resolve_type(&col_types[i]);
            ColumnSchema {
                name,
                hyper_type: inferred_to_hyper_name(&resolved),
                nullable: true, // CSV columns are always nullable
            }
        })
        .collect();

    Ok(columns)
}

/// Second-pass streaming widen: re-read the given CSV source and, for columns
/// the first-pass inference classified as `INT`, `BIGINT`, or `DOUBLE PRECISION`,
/// promote the type if a value outside its current range appears anywhere in
/// the file (not just the first 1 000 rows).
///
/// The first pass handles column naming and ambiguous-type resolution; this
/// pass exists specifically to catch "big value hidden near the end of a CSV"
/// — the exact bug that prompted this code path, where OWID keeps world-
/// aggregate populations (~8 billion) in the last rows of a file whose first
/// thousand rows only contain country-sized numbers.
///
/// Promotion rules per numeric column:
/// * `INT` → `BIGINT` if any value exceeds `i32` range.
/// * `INT` / `BIGINT` → `NUMERIC(38,0)` if any value exceeds `i64` range.
/// * `INT` / `BIGINT` → `DOUBLE PRECISION` if any value contains a decimal point
///   or exponent (mixed integer/float column).
///
/// Columns with non-numeric inferred types are left untouched. Nullability is
/// preserved. Empty fields are ignored.
///
/// # Errors
///
/// Returns [`ErrorCode::SchemaMismatch`] when the CSV parser fails on
/// any row (typically unbalanced quotes, mismatched delimiters, or
/// non-UTF-8 bytes reported by the `csv` crate).
///
/// # Panics
///
/// Does not panic in practice. The `stats.get_mut(&col_idx).expect("preallocated")`
/// invariant holds because `stats` is preallocated with one entry per
/// `candidate_idxs` value, and the loop only uses indices from that
/// same slice.
pub fn widen_csv_numeric_columns<R: std::io::Read>(
    reader: R,
    has_header: bool,
    columns: &mut [ColumnSchema],
) -> Result<(), McpError> {
    // Only bother if at least one column is a candidate for widening.
    let candidate_idxs: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            matches!(
                c.hyper_type.as_str(),
                "INT" | "INTEGER" | "BIGINT" | "DOUBLE PRECISION"
            )
        })
        .map(|(i, _)| i)
        .collect();
    if candidate_idxs.is_empty() {
        return Ok(());
    }

    // Per-column observed-value state. `min`/`max` track integer extrema as
    // `i128` to cover everything up to `i64::MIN`..`i64::MAX` plus headroom for
    // overflow detection. `has_decimal` flips true when any field looks like a
    // float (contains `.`, `e`, or `E`) so we can promote to DOUBLE.
    #[derive(Default)]
    struct ColStats {
        min: Option<i128>,
        max: Option<i128>,
        has_decimal: bool,
        overflow_i128: bool,
    }
    let mut stats: std::collections::HashMap<usize, ColStats> = candidate_idxs
        .iter()
        .map(|i| (*i, ColStats::default()))
        .collect();

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(has_header)
        .from_reader(reader);

    for (row_idx, result) in rdr.records().enumerate() {
        let record = result.map_err(|e| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("CSV parse error at row {}: {e}", row_idx + 1),
            )
        })?;
        for &col_idx in &candidate_idxs {
            let Some(field) = record.get(col_idx) else {
                continue;
            };
            let trimmed = field.trim();
            if trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("null")
                || trimmed.eq_ignore_ascii_case("na")
            {
                continue;
            }
            let s = stats.get_mut(&col_idx).expect("preallocated");
            if trimmed.contains('.') || trimmed.contains('e') || trimmed.contains('E') {
                s.has_decimal = true;
                continue;
            }
            match trimmed.parse::<i128>() {
                Ok(n) => {
                    s.min = Some(s.min.map_or(n, |m| m.min(n)));
                    s.max = Some(s.max.map_or(n, |m| m.max(n)));
                }
                Err(_) => {
                    // Non-decimal, non-integer value in a numeric column. Leave
                    // widening to the first-pass classifier (it will already
                    // have picked TEXT if this happened inside the sample) and
                    // just skip. Any truly ambiguous column collapses there.
                    s.overflow_i128 = true;
                }
            }
        }
    }

    for (&col_idx, s) in &stats {
        let col = &mut columns[col_idx];
        let i32_range = i128::from(i32::MIN)..=i128::from(i32::MAX);
        let i64_range = i128::from(i64::MIN)..=i128::from(i64::MAX);
        match col.hyper_type.as_str() {
            "INT" | "INTEGER" => {
                if s.has_decimal {
                    col.hyper_type = "DOUBLE PRECISION".into();
                } else if s.overflow_i128
                    || !s.min.is_none_or(|m| i64_range.contains(&m))
                    || !s.max.is_none_or(|m| i64_range.contains(&m))
                {
                    col.hyper_type = "NUMERIC(38,0)".into();
                } else if !s.min.is_none_or(|m| i32_range.contains(&m))
                    || !s.max.is_none_or(|m| i32_range.contains(&m))
                {
                    col.hyper_type = "BIGINT".into();
                }
            }
            "BIGINT" => {
                if s.has_decimal {
                    col.hyper_type = "DOUBLE PRECISION".into();
                } else if s.overflow_i128
                    || !s.min.is_none_or(|m| i64_range.contains(&m))
                    || !s.max.is_none_or(|m| i64_range.contains(&m))
                {
                    col.hyper_type = "NUMERIC(38,0)".into();
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Classify a single CSV field value. Empty strings, `"null"`, and `"NA"`
/// (case-insensitive) are treated as null. Boolean literals, integers, floats,
/// and ISO date/timestamp patterns are recognized before falling back to TEXT.
fn infer_csv_field_type(field: &str) -> InferredType {
    let trimmed = field.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("null")
        || trimmed.eq_ignore_ascii_case("na")
    {
        return InferredType::Null;
    }
    if trimmed.eq_ignore_ascii_case("true") || trimmed.eq_ignore_ascii_case("false") {
        return InferredType::Bool;
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        if i32::try_from(i).is_ok() {
            return InferredType::Int;
        }
        return InferredType::BigInt;
    }
    if trimmed.parse::<f64>().is_ok() {
        return InferredType::Double;
    }
    infer_string_type(trimmed)
}

/// Parse a user-provided schema override (`{"column_name": "TYPE", ...}`) into
/// a `Vec<ColumnSchema>`. Validates each type name against [`map_hyper_type`] and
/// rejects unknown types early. All override columns are marked nullable.
///
/// This variant treats the override as a complete schema: it becomes the full
/// `Vec<ColumnSchema>` in whatever order the JSON object iterates. Used mostly
/// by tests and code paths without an inferred schema to merge onto; prefer
/// [`apply_schema_override`] for ingest paths so columns stay aligned with the
/// source file's header order.
///
/// # Errors
///
/// - Returns [`ErrorCode::SchemaMismatch`] if any value in `schema` is
///   not a string.
/// - Returns [`ErrorCode::SchemaMismatch`] if any type name does not
///   resolve via [`map_hyper_type`].
pub fn parse_schema_override(
    schema: &serde_json::Map<String, Value>,
) -> Result<Vec<ColumnSchema>, McpError> {
    let mut columns = Vec::new();
    for (name, type_val) in schema {
        let type_name = type_val.as_str().ok_or_else(|| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Schema type for '{name}' must be a string"),
            )
        })?;
        if map_hyper_type(type_name).is_none() {
            return Err(McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Unknown type '{type_name}' for column '{name}'"),
            ));
        }
        columns.push(ColumnSchema {
            name: name.clone(),
            hyper_type: type_name.to_uppercase(),
            nullable: true,
        });
    }
    Ok(columns)
}

/// Normalize a raw MCP `schema` parameter value into the column-name → type
/// map expected by [`apply_schema_override`] and [`parse_schema_override`].
///
/// The MCP tool parameter is declared as `Option<serde_json::Value>` so the
/// `rmcp` / `schemars` pipeline emits a permissive `true` JSON Schema. In
/// practice some MCP clients forward this field as a **JSON-encoded string**
/// rather than a raw JSON object — e.g. Windsurf/Cascade serializes
/// `{"postal_code": "TEXT"}` as `"\"{\\\"postal_code\\\": \\\"TEXT\\\"}\""`.
/// Both a JSON object and a JSON-encoded string containing an object are
/// accepted, so the override is honored whichever shape the client forwards.
///
/// Accepted shapes:
///
/// * `None` / `Some(Value::Null)` — no override.
/// * `Some(Value::Object(m))` — used directly.
/// * `Some(Value::String(s))` — `s` is parsed as JSON; must decode to an
///   object. A non-object payload (array, number, etc.) is rejected with
///   `SchemaMismatch` so the caller gets a clear error rather than a silent
///   no-op.
///
/// Any other shape is rejected with `SchemaMismatch` for the same reason.
///
/// # Errors
///
/// - Returns [`ErrorCode::SchemaMismatch`] if a `Value::String` payload
///   is non-empty but not valid JSON, or if it decodes to anything
///   other than an object or null.
/// - Returns [`ErrorCode::SchemaMismatch`] for any other `Value` shape
///   (boolean, number, array, etc.).
pub fn normalize_schema_param(
    schema: Option<&Value>,
) -> Result<Option<serde_json::Map<String, Value>>, McpError> {
    let Some(v) = schema else {
        return Ok(None);
    };
    match v {
        Value::Null => Ok(None),
        Value::Object(m) => Ok(Some(m.clone())),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let parsed: Value = serde_json::from_str(trimmed).map_err(|e| {
                McpError::new(
                    ErrorCode::SchemaMismatch,
                    format!(
                        "`schema` parameter is a string but not valid JSON: {e}. \
                         Expected an object like {{\"col\": \"TEXT\"}}."
                    ),
                )
            })?;
            match parsed {
                Value::Object(m) => Ok(Some(m)),
                Value::Null => Ok(None),
                other => Err(McpError::new(
                    ErrorCode::SchemaMismatch,
                    format!(
                        "`schema` parameter must be a JSON object mapping column names \
                         to type strings, got {}.",
                        json_type_name(&other)
                    ),
                )),
            }
        }
        other => Err(McpError::new(
            ErrorCode::SchemaMismatch,
            format!(
                "`schema` parameter must be a JSON object mapping column names to type \
                 strings, got {}.",
                json_type_name(other)
            ),
        )),
    }
}

/// Short human-readable name of a JSON value's kind for error messages.
pub(crate) fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Overlay a user-provided override (`{"column_name": "TYPE", ...}`) on top of
/// an already-inferred column list, preserving the inferred column **order** and
/// replacing only the types listed in the override.
///
/// This is the semantics used by all ingest paths (`load_file`, `load_data`,
/// `query_file`, `query_data`): the source file's header determines column order
/// and the set of columns; the override is a partial name→type dictionary that
/// lets callers force a wider type (e.g. `{"Population": "BIGINT"}`) without
/// enumerating every column.
///
/// Unknown override keys are rejected with a `SchemaMismatch` error that lists
/// the real column names so LLMs can self-correct without a round-trip.
///
/// # Errors
///
/// Returns [`ErrorCode::SchemaMismatch`] when:
/// - Any override value is not a string.
/// - Any override type name fails [`map_hyper_type`] resolution.
/// - An override key names a column that is not present in `inferred`
///   (the error lists the real column names).
pub fn apply_schema_override(
    mut inferred: Vec<ColumnSchema>,
    override_map: &serde_json::Map<String, Value>,
) -> Result<Vec<ColumnSchema>, McpError> {
    // Validate: every override key must match a real inferred column and every
    // override value must be a known type string.
    let known: std::collections::HashSet<&str> = inferred.iter().map(|c| c.name.as_str()).collect();
    for (name, type_val) in override_map {
        if !known.contains(name.as_str()) {
            let real: Vec<&str> = inferred.iter().map(|c| c.name.as_str()).collect();
            return Err(McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Override key '{name}' does not match any column. Known columns: {real:?}"),
            ));
        }
        let type_name = type_val.as_str().ok_or_else(|| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Schema type for '{name}' must be a string"),
            )
        })?;
        if map_hyper_type(type_name).is_none() {
            return Err(McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Unknown type '{type_name}' for column '{name}'"),
            ));
        }
    }

    // Apply: overlay types by column name. Column order is the inferred order.
    for col in &mut inferred {
        if let Some(v) = override_map.get(&col.name).and_then(|v| v.as_str()) {
            col.hyper_type = v.trim().to_uppercase();
        }
    }
    Ok(inferred)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_string_type_recognizes_iso_dates_and_timestamps() {
        assert_eq!(infer_string_type("2024-01-15"), InferredType::Date);
        assert_eq!(
            infer_string_type("2024-01-15T13:45:30"),
            InferredType::Timestamp
        );
        assert_eq!(infer_string_type("hello"), InferredType::Text);
    }

    #[test]
    fn infer_string_type_does_not_panic_on_non_ascii() {
        // 10 bytes, '-' at char 4 and 7, but `s[0..4]` lands inside 'é'.
        assert_eq!(infer_string_type("abcé-xy-z"), InferredType::Text);
        // `s[0..10]` lands inside the 'é' that precedes the 'T' (char 10).
        assert_eq!(infer_string_type("abcdefghiéT12345678"), InferredType::Text);
        // Multibyte chars before the date/timestamp separators are not ISO 8601.
        assert_eq!(infer_string_type("é024-01-15T13:45:30"), InferredType::Text);
        assert_eq!(infer_string_type("2024-01-15T13:45:3é"), InferredType::Text);
        assert_eq!(
            infer_string_type("日本語日本語日本語日本語"),
            InferredType::Text
        );
    }
}
