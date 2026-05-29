// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

#![allow(
    clippy::cast_precision_loss,
    reason = "diagnostic metric output; bounded chunk sizes"
)]

use std::sync::{Arc, Mutex};

use napi::bindgen_prelude::*;
use napi_derive::napi;
use tokio::sync::mpsc;

use crate::result::ResultColumnInfo;

// =============================================================================
// ColumnData — typed storage for a single column's values
// =============================================================================

/// Internal typed storage for column data extracted from a Hyper chunk.
#[derive(Debug)]
pub(crate) enum ColumnData {
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Strings(Vec<String>),
}

// =============================================================================
// ColumnarChunk — exposed to JS
// =============================================================================

/// A chunk of query results stored in columnar format for high-performance access.
///
/// Instead of per-row, per-cell accessor calls, columnar chunks let you retrieve
/// an entire column at once as a flat array. This reduces JS↔Rust FFI overhead
/// from O(rows × columns) to O(columns).
///
/// @example
/// ```js
/// const stream = conn.executeQueryColumnar('SELECT id, value FROM t');
/// let chunk;
/// while ((chunk = await stream.nextChunk()) !== null) {
///   const ids = chunk.getInt32Column(0);       // number[]
///   const values = chunk.getFloat64Column(1);  // number[]
///   const nulls = chunk.getNulls(1);           // boolean[]
///   // Process arrays in bulk — no per-row FFI!
/// }
/// ```
#[napi]
#[derive(Debug)]
pub struct ColumnarChunk {
    columns: Vec<ColumnData>,
    /// Per-column null bitmaps. true = value is null.
    null_bitmaps: Vec<Vec<bool>>,
    row_count: usize,
}

#[napi]
impl ColumnarChunk {
    /// Number of rows in this chunk.
    #[napi(getter)]
    pub fn row_count(&self) -> u32 {
        // A single chunk is bounded by the server's chunk size (millions, not
        // billions); this field never approaches u32::MAX in practice.
        u32::try_from(self.row_count).unwrap_or(u32::MAX)
    }

    /// Number of columns.
    #[napi(getter)]
    pub fn column_count(&self) -> u32 {
        // Result schemas with more than u32::MAX columns are not representable
        // in Hyper; saturating is a safe diagnostic.
        u32::try_from(self.columns.len()).unwrap_or(u32::MAX)
    }

    /// Returns an entire column as an array of 32-bit integers.
    ///
    /// Null values are represented as 0. Use `getNulls(index)` to distinguish nulls.
    #[napi]
    pub fn get_int32_column(&self, index: u32) -> Result<Vec<i32>> {
        match self.columns.get(index as usize) {
            Some(ColumnData::Int32(v)) => Ok(v.clone()),
            #[expect(
                clippy::cast_possible_truncation,
                reason = "caller-selected column coercion: `get_int32_column` is documented to narrow Int64/Float64 columns into Int32"
            )]
            Some(ColumnData::Int64(v)) => Ok(v.iter().map(|&x| x as i32).collect()),
            #[expect(
                clippy::cast_possible_truncation,
                reason = "caller-selected column coercion: `get_int32_column` is documented to narrow Int64/Float64 columns into Int32"
            )]
            Some(ColumnData::Float64(v)) => Ok(v.iter().map(|&x| x as i32).collect()),
            Some(ColumnData::Strings(_)) => Err(Error::from_reason("Column is STRING, not INT32")),
            None => Err(Error::from_reason(format!(
                "Column index {index} out of range"
            ))),
        }
    }

    /// Returns an entire column as an array of 64-bit floats.
    ///
    /// Null values are represented as 0.0. Use `getNulls(index)` to distinguish nulls.
    #[napi]
    pub fn get_float64_column(&self, index: u32) -> Result<Vec<f64>> {
        match self.columns.get(index as usize) {
            Some(ColumnData::Float64(v)) => Ok(v.clone()),
            Some(ColumnData::Int32(v)) => Ok(v.iter().map(|&x| f64::from(x)).collect()),
            Some(ColumnData::Int64(v)) => Ok(v.iter().map(|&x| x as f64).collect()),
            Some(ColumnData::Strings(_)) => {
                Err(Error::from_reason("Column is STRING, not FLOAT64"))
            }
            None => Err(Error::from_reason(format!(
                "Column index {index} out of range"
            ))),
        }
    }

    /// Returns an entire column as an array of 64-bit integers (as JS numbers).
    ///
    /// Note: JS numbers lose precision above 2^53.
    /// Null values are represented as 0. Use `getNulls(index)` to distinguish nulls.
    #[napi]
    pub fn get_int64_column(&self, index: u32) -> Result<Vec<i64>> {
        match self.columns.get(index as usize) {
            Some(ColumnData::Int64(v)) => Ok(v.clone()),
            Some(ColumnData::Int32(v)) => Ok(v.iter().map(|&x| i64::from(x)).collect()),
            #[expect(
                clippy::cast_possible_truncation,
                reason = "caller-selected column coercion: `get_int64_column` is documented to narrow Float64 columns into Int64"
            )]
            Some(ColumnData::Float64(v)) => Ok(v.iter().map(|&x| x as i64).collect()),
            Some(ColumnData::Strings(_)) => Err(Error::from_reason("Column is STRING, not INT64")),
            None => Err(Error::from_reason(format!(
                "Column index {index} out of range"
            ))),
        }
    }

    /// Returns an entire column as an array of strings.
    #[napi]
    pub fn get_string_column(&self, index: u32) -> Result<Vec<String>> {
        match self.columns.get(index as usize) {
            Some(ColumnData::Strings(v)) => Ok(v.clone()),
            Some(ColumnData::Int32(v)) => {
                Ok(v.iter().map(std::string::ToString::to_string).collect())
            }
            Some(ColumnData::Int64(v)) => {
                Ok(v.iter().map(std::string::ToString::to_string).collect())
            }
            Some(ColumnData::Float64(v)) => {
                Ok(v.iter().map(std::string::ToString::to_string).collect())
            }
            None => Err(Error::from_reason(format!(
                "Column index {index} out of range"
            ))),
        }
    }

    /// Returns a boolean array indicating which rows are null for the given column.
    ///
    /// `true` means the value is null.
    #[napi]
    pub fn get_nulls(&self, index: u32) -> Result<Vec<bool>> {
        match self.null_bitmaps.get(index as usize) {
            Some(v) => Ok(v.clone()),
            None => Err(Error::from_reason(format!(
                "Column index {index} out of range"
            ))),
        }
    }
}

// =============================================================================
// Columnar extraction from Hyper chunks
// =============================================================================

/// Extracts a Hyper chunk into columnar format.
pub(crate) fn extract_chunk_columnar(
    chunk: &[hyperdb_api::Row],
    schema: &hyperdb_api::ResultSchema,
) -> ColumnarChunk {
    let row_count = chunk.len();
    let col_count = schema.column_count();

    // Pre-allocate columnar storage based on schema types
    let mut columns: Vec<ColumnData> = Vec::with_capacity(col_count);
    let mut null_bitmaps: Vec<Vec<bool>> = Vec::with_capacity(col_count);

    for col_idx in 0..col_count {
        let sql_type = schema.column(col_idx).sql_type();
        null_bitmaps.push(vec![false; row_count]);

        match sql_type {
            hyperdb_api::SqlType::Bool
            | hyperdb_api::SqlType::SmallInt
            | hyperdb_api::SqlType::Int
            | hyperdb_api::SqlType::Oid => {
                columns.push(ColumnData::Int32(vec![0i32; row_count]));
            }
            hyperdb_api::SqlType::BigInt => {
                columns.push(ColumnData::Int64(vec![0i64; row_count]));
            }
            hyperdb_api::SqlType::Float
            | hyperdb_api::SqlType::Double
            | hyperdb_api::SqlType::Numeric { .. } => {
                columns.push(ColumnData::Float64(vec![0.0f64; row_count]));
            }
            _ => {
                columns.push(ColumnData::Strings(vec![String::new(); row_count]));
            }
        }
    }

    // Pre-compute sql_types to avoid repeated schema lookups in the inner loop
    let sql_types: Vec<hyperdb_api::SqlType> = (0..col_count)
        .map(|i| schema.column(i).sql_type())
        .collect();

    // Fill columns from rows — tight loop, no FFI per cell
    for (row_idx, row) in chunk.iter().enumerate() {
        for col_idx in 0..col_count {
            if row.is_null(col_idx) {
                null_bitmaps[col_idx][row_idx] = true;
                continue;
            }

            match sql_types[col_idx] {
                hyperdb_api::SqlType::Bool => {
                    if let ColumnData::Int32(ref mut v) = columns[col_idx] {
                        v[row_idx] = i32::from(row.get_bool(col_idx).unwrap_or(false));
                    }
                }
                hyperdb_api::SqlType::SmallInt => {
                    if let ColumnData::Int32(ref mut v) = columns[col_idx] {
                        v[row_idx] = i32::from(row.get_i16(col_idx).unwrap_or(0));
                    }
                }
                hyperdb_api::SqlType::Int | hyperdb_api::SqlType::Oid => {
                    if let ColumnData::Int32(ref mut v) = columns[col_idx] {
                        v[row_idx] = row.get_i32(col_idx).unwrap_or(0);
                    }
                }
                hyperdb_api::SqlType::BigInt => {
                    if let ColumnData::Int64(ref mut v) = columns[col_idx] {
                        v[row_idx] = row.get_i64(col_idx).unwrap_or(0);
                    }
                }
                hyperdb_api::SqlType::Float => {
                    if let ColumnData::Float64(ref mut v) = columns[col_idx] {
                        v[row_idx] = f64::from(row.get_f32(col_idx).unwrap_or(0.0));
                    }
                }
                hyperdb_api::SqlType::Double => {
                    if let ColumnData::Float64(ref mut v) = columns[col_idx] {
                        v[row_idx] = row.get_f64(col_idx).unwrap_or(0.0);
                    }
                }
                hyperdb_api::SqlType::Numeric { .. } => {
                    // Schema-aware decode then narrow to f64. `get_f64` must NOT
                    // be used: it reinterprets the unscaled-integer bytes as an
                    // IEEE-754 double (garbage/NaN). The columnar fast path
                    // surfaces numerics as f64 (lossy for >15 sig digits); the
                    // row-wise path preserves exact text via `getString`.
                    if let ColumnData::Float64(ref mut v) = columns[col_idx] {
                        v[row_idx] = row.get_numeric(col_idx).map_or(0.0, |n| n.to_f64());
                    }
                }
                hyperdb_api::SqlType::Date => {
                    if let ColumnData::Strings(ref mut v) = columns[col_idx] {
                        v[row_idx] = row
                            .get_date(col_idx)
                            .map(|d| d.to_string())
                            .unwrap_or_default();
                    }
                }
                hyperdb_api::SqlType::Timestamp | hyperdb_api::SqlType::TimestampTz => {
                    if let ColumnData::Strings(ref mut v) = columns[col_idx] {
                        v[row_idx] = row
                            .get_timestamp(col_idx)
                            .map(|t| t.to_string())
                            .unwrap_or_default();
                    }
                }
                _ => {
                    if let ColumnData::Strings(ref mut v) = columns[col_idx] {
                        v[row_idx] = row.get_string(col_idx).unwrap_or_default();
                    }
                }
            }
        }
    }

    ColumnarChunk {
        columns,
        null_bitmaps,
        row_count,
    }
}

// =============================================================================
// ColumnarStream — streaming columnar query results
// =============================================================================

type ColumnarChunkResult = std::result::Result<ColumnarChunk, String>;

/// A streaming query result that returns columnar chunks for high-performance bulk access.
///
/// Each chunk contains typed arrays per column instead of per-row objects,
/// reducing FFI overhead from O(rows × columns) to O(columns).
///
/// @example
/// ```js
/// const stream = conn.executeQueryColumnar('SELECT id, value FROM measurements');
/// let chunk;
/// while ((chunk = await stream.nextChunk()) !== null) {
///   const ids = chunk.getInt32Column(0);
///   const values = chunk.getFloat64Column(1);
///   // Process in bulk...
/// }
/// ```
#[napi]
#[derive(Debug)]
pub struct ColumnarStream {
    receiver: Mutex<Option<mpsc::Receiver<ColumnarChunkResult>>>,
    schema: Arc<Mutex<Option<Vec<ResultColumnInfo>>>>,
}

#[napi]
impl ColumnarStream {
    /// Returns the next columnar chunk, or `null` when all rows have been consumed.
    #[napi]
    pub async fn next_chunk(&self) -> Result<Option<ColumnarChunk>> {
        let mut rx = {
            let mut guard = self
                .receiver
                .lock()
                .map_err(|e| Error::from_reason(format!("Lock poisoned: {e}")))?;
            match guard.take() {
                Some(rx) => rx,
                None => return Ok(None),
            }
        };

        let result = rx.recv().await;

        match result {
            Some(Ok(chunk)) => {
                let mut guard = self
                    .receiver
                    .lock()
                    .map_err(|e| Error::from_reason(format!("Lock poisoned: {e}")))?;
                *guard = Some(rx);
                Ok(Some(chunk))
            }
            Some(Err(e)) => Err(Error::from_reason(e)),
            None => Ok(None),
        }
    }

    /// Returns column metadata for this result set.
    #[napi]
    pub fn get_schema(&self) -> Result<Option<Vec<ResultColumnInfo>>> {
        let guard = self
            .schema
            .lock()
            .map_err(|e| Error::from_reason(format!("Lock poisoned: {e}")))?;
        Ok(guard.clone())
    }

    /// Cancels the stream, releasing the background reader task.
    ///
    /// After calling this, `nextChunk()` returns `null`. Call this when you
    /// no longer need the remaining chunks to free resources immediately
    /// rather than waiting for garbage collection.
    #[napi]
    pub fn cancel(&self) -> Result<()> {
        let mut guard = self
            .receiver
            .lock()
            .map_err(|e| Error::from_reason(format!("Lock poisoned: {e}")))?;
        *guard = None;
        Ok(())
    }
}

/// Spawns an async task that reads chunks from the connection, converts
/// them to columnar format, and sends them through a bounded channel.
pub(crate) fn start_columnar_stream(
    conn: Arc<hyperdb_api::AsyncConnection>,
    sql: String,
) -> ColumnarStream {
    let (tx, rx) = mpsc::channel::<ColumnarChunkResult>(4);
    let schema_holder: Arc<Mutex<Option<Vec<ResultColumnInfo>>>> = Arc::new(Mutex::new(None));
    let schema_for_stream = Arc::clone(&schema_holder);

    // napi-rs 3: use the napi-managed tokio runtime (see query_stream.rs
    // for the rationale — raw `tokio::spawn` panics when called from a
    // synchronous napi callback).
    napi::bindgen_prelude::spawn(async move {
        let mut rowset = match conn.execute_query(&sql).await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Err(e.to_string())).await;
                return;
            }
        };

        let mut schema: Option<hyperdb_api::ResultSchema> = None;

        loop {
            let chunk = match rowset.next_chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                    return;
                }
            };

            if schema.is_none() {
                schema = rowset.schema();
                if let Some(ref s) = schema {
                    let info: Vec<ResultColumnInfo> = s
                        .columns()
                        .iter()
                        .map(|col| ResultColumnInfo {
                            name: col.name().to_string(),
                            type_name: col.sql_type().to_string(),
                            // Column count in a result schema is structurally
                            // bounded by Hyper (far below u32::MAX).
                            index: u32::try_from(col.index()).unwrap_or(u32::MAX),
                        })
                        .collect();
                    if let Ok(mut guard) = schema_holder.lock() {
                        *guard = Some(info);
                    }
                }
            }

            let Some(s) = schema.as_ref() else {
                let _ = tx.send(Err("No schema available".to_string())).await;
                return;
            };

            let columnar_chunk = extract_chunk_columnar(&chunk, s);

            if tx.send(Ok(columnar_chunk)).await.is_err() {
                return;
            }
        }
    });

    ColumnarStream {
        receiver: Mutex::new(Some(rx)),
        schema: schema_for_stream,
    }
}
