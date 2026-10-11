// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use napi::Unknown;
use napi::bindgen_prelude::*;
use napi_derive::napi;

#[allow(
    unused_imports,
    reason = "imported for use in doc comments that reference the type path"
)]
use crate::connection::Connection;
#[allow(
    unused_imports,
    reason = "imported for use in doc comments that reference the type path"
)]
use crate::types::TableDefinition;

// =============================================================================
// RowInserter
// =============================================================================

/// Row-at-a-time bulk inserter backed by `HyperBinary` COPY.
///
/// Rows are buffered in memory; `execute()` encodes them into Hyper's
/// binary COPY format and ships them to the server on a tokio task.
/// For Arrow IPC data, use `ArrowInserter`
/// instead — it bypasses the per-row JS encoding loop entirely.
#[napi]
#[derive(Debug)]
pub struct RowInserter {
    conn: Arc<hyperdb_api::AsyncConnection>,
    table_def: hyperdb_api::TableDefinition,
    /// Interior mutability so async `execute()` can take &self instead of &mut self.
    rows: Mutex<Vec<Vec<InsertValue>>>,
}

/// Internal representation of a value to insert.
#[derive(Clone, Debug)]
pub(crate) enum InsertValue {
    Null,
    Bool(bool),
    I32(i32),
    I64(i64),
    F64(f64),
    String(String),
    Bytes(Vec<u8>),
}

#[napi]
impl RowInserter {
    /// Creates a new `RowInserter` for the given connection and table definition.
    #[napi(constructor)]
    pub fn new(connection: &Connection, table_def: &TableDefinition) -> Self {
        RowInserter {
            conn: connection.inner_arc(),
            table_def: table_def.inner.clone(),
            rows: Mutex::new(Vec::new()),
        }
    }

    /// Adds a row of values to the insert buffer.
    #[napi(ts_args_type = "values: Array<number | string | boolean | null | Buffer>")]
    pub fn add_row(&self, _env: Env, values: Unknown) -> Result<()> {
        let arr = values.coerce_to_object()?;
        let length: u32 = arr.get_named_property("length")?;

        let mut row = Vec::with_capacity(length as usize);
        for i in 0..length {
            let val: Unknown = arr.get_element(i)?;
            let insert_val = js_value_to_insert_value(val)?;
            row.push(insert_val);
        }

        self.rows
            .lock()
            .map_err(|e| Error::from_reason(format!("Lock poisoned: {e}")))?
            .push(row);
        Ok(())
    }

    /// Adds multiple rows at once.
    #[napi(ts_args_type = "rows: Array<Array<number | string | boolean | null | Buffer>>")]
    pub fn add_rows(&self, _env: Env, rows: Unknown) -> Result<()> {
        let arr = rows.coerce_to_object()?;
        let length: u32 = arr.get_named_property("length")?;

        let mut all_rows = Vec::with_capacity(length as usize);
        for i in 0..length {
            let row_val: Unknown = arr.get_element(i)?;
            let row_arr = row_val.coerce_to_object()?;
            let row_len: u32 = row_arr.get_named_property("length")?;

            let mut row = Vec::with_capacity(row_len as usize);
            for j in 0..row_len {
                let val: Unknown = row_arr.get_element(j)?;
                let insert_val = js_value_to_insert_value(val)?;
                row.push(insert_val);
            }

            all_rows.push(row);
        }

        self.rows
            .lock()
            .map_err(|e| Error::from_reason(format!("Lock poisoned: {e}")))?
            .extend(all_rows);
        Ok(())
    }

    /// Returns the number of buffered rows.
    #[napi(getter)]
    pub fn buffered_row_count(&self) -> u32 {
        self.rows
            .lock()
            .map_or(0, |r| u32::try_from(r.len()).unwrap_or(u32::MAX))
    }

    /// Adds rows from columnar (per-column) typed arrays.
    ///
    /// Significantly faster than `addRows()` for numeric data — avoids
    /// per-cell JS type detection.
    #[napi(
        ts_args_type = "int32Columns: Record<number, number[]>, float64Columns: Record<number, number[]>, int64Columns: Record<number, number[]>, rowCount: number"
    )]
    #[allow(
        clippy::used_underscore_binding,
        reason = "underscore-prefixed parameter retained for trait-method signature compatibility"
    )]
    #[allow(
        clippy::needless_pass_by_value,
        reason = "call-site ergonomics: function consumes logically-owned parameters, refactoring signatures is not worth per-site churn"
    )]
    pub fn add_columnar(
        &self,
        _env: Env,
        int32_columns: Object,
        float64_columns: Object,
        int64_columns: Object,
        row_count: u32,
    ) -> Result<()> {
        let row_count = row_count as usize;

        let int32_cols = parse_number_columns(&_env, &int32_columns)?;
        let float64_cols = parse_number_columns(&_env, &float64_columns)?;
        let int64_cols = parse_number_columns(&_env, &int64_columns)?;

        let col_count = self.table_def.columns().len();
        let mut all_rows = Vec::with_capacity(row_count);

        for row_idx in 0..row_count {
            let mut row = Vec::with_capacity(col_count);
            for col_idx in 0..col_count {
                if let Some(arr) = int32_cols.get(&col_idx) {
                    if row_idx < arr.len() {
                        // JS Number → Int32 column: caller explicitly chose
                        // the `int32Columns` bucket, so narrowing the f64
                        // element is the documented behavior of this path.
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "caller placed value in `int32Columns` → value is asserted to fit in i32; narrowing f64 is the documented coercion for this columnar path"
                        )]
                        let narrowed = arr[row_idx] as i32;
                        row.push(InsertValue::I32(narrowed));
                    } else {
                        row.push(InsertValue::Null);
                    }
                } else if let Some(arr) = float64_cols.get(&col_idx) {
                    if row_idx < arr.len() {
                        row.push(InsertValue::F64(arr[row_idx]));
                    } else {
                        row.push(InsertValue::Null);
                    }
                } else if let Some(arr) = int64_cols.get(&col_idx) {
                    if row_idx < arr.len() {
                        // JS Number → Int64 column: JS numbers past 2^53 lose
                        // integer precision, so the caller is responsible for
                        // providing in-range values; this is the documented
                        // behavior of the `int64Columns` path.
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "caller placed value in `int64Columns` → value is asserted to fit in i64; narrowing f64 is the documented coercion for this columnar path"
                        )]
                        let narrowed = arr[row_idx] as i64;
                        row.push(InsertValue::I64(narrowed));
                    } else {
                        row.push(InsertValue::Null);
                    }
                } else {
                    row.push(InsertValue::Null);
                }
            }
            all_rows.push(row);
        }

        self.rows
            .lock()
            .map_err(|e| Error::from_reason(format!("Lock poisoned: {e}")))?
            .extend(all_rows);
        Ok(())
    }

    /// Sends all buffered rows to the server and returns the number of rows inserted.
    ///
    /// `HyperBinary` encoding is CPU-bound; we run it on tokio's blocking
    /// pool so the event loop stays free for other requests. The actual
    /// socket I/O after encoding runs on the async pool.
    #[napi]
    pub async fn execute(&self) -> Result<i64> {
        let conn = Arc::clone(&self.conn);
        let table_def = self.table_def.clone();
        let rows = {
            let mut guard = self
                .rows
                .lock()
                .map_err(|e| Error::from_reason(format!("Lock poisoned: {e}")))?;
            std::mem::take(&mut *guard)
        };

        // Encode the staged rows into a HyperBinary buffer off the async
        // runtime. This is pure CPU work, no awaits are helpful.
        let td_for_encoding = table_def.clone();
        let encoded = tokio::task::spawn_blocking(move || encode_rows(&rows, &td_for_encoding))
            .await
            .map_err(|e| Error::from_reason(format!("Task join error: {e}")))?
            .map_err(Error::from_reason)?;

        if encoded.is_empty() {
            return Ok(0);
        }

        // Stream the pre-encoded buffer as one COPY IN session.
        let count = conn
            .copy_in_hyperbinary(&table_def, &encoded)
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?;
        i64::try_from(count).map_err(|_| Error::from_reason("row count exceeds i64::MAX"))
    }
}

/// Encodes staged rows into a `HyperBinary` COPY buffer using the sync
/// [`hyperdb_api::InsertChunk`] encoder — the same format the server
/// expects over COPY IN.
fn encode_rows(
    rows: &[Vec<InsertValue>],
    table_def: &hyperdb_api::TableDefinition,
) -> std::result::Result<Vec<u8>, String> {
    use hyperdb_api::InsertChunk;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut chunk = InsertChunk::from_table_definition(table_def);
    let columns: &[hyperdb_api::ColumnDefinition] = table_def.columns();

    for row in rows {
        for (col_idx, val) in row.iter().enumerate() {
            let col_type = columns
                .get(col_idx)
                .and_then(hyperdb_api::ColumnDefinition::sql_type);
            add_value_typed(&mut chunk, val, col_type)
                .map_err(|e| format!("row encoding error: {e}"))?;
        }
        chunk
            .end_row()
            .map_err(|e| format!("row boundary error: {e}"))?;
    }

    match chunk.take() {
        Some(buf) => Ok(buf.to_vec()),
        None => Ok(Vec::new()),
    }
}

/// Narrows an `i64` to `i16` for a `SMALLINT` destination column, or fails.
///
/// Truncating with `as` would be silent data corruption: `add_i16` encodes two
/// bytes, so a wrapped value is a *valid* encoding of the wrong number and
/// Hyper cannot detect it.
fn narrow_i16(v: i64) -> std::result::Result<i16, hyperdb_api::Error> {
    i16::try_from(v).map_err(|_| {
        hyperdb_api::Error::invalid_operation(format!(
            "value {v} does not fit the destination SMALLINT column (valid range {}..={})",
            i16::MIN,
            i16::MAX
        ))
    })
}

/// Narrows an `i64` to `i32` for an `INT` destination column, or fails.
///
/// See [`narrow_i16`] for why this cannot use `as`.
fn narrow_i32(v: i64) -> std::result::Result<i32, hyperdb_api::Error> {
    i32::try_from(v).map_err(|_| {
        hyperdb_api::Error::invalid_operation(format!(
            "value {v} does not fit the destination INT column (valid range {}..={})",
            i32::MIN,
            i32::MAX
        ))
    })
}

/// Encodes an `InsertValue` according to the destination column's SQL type.
///
/// The JS value was classified by `js_value_to_insert_value` (which bins based
/// on JS-side integer/float detection), but the destination column's SQL type
/// governs the final binary encoding.
///
/// **Integer-to-integer narrowing is rejected, not truncated.** An earlier
/// version of this function claimed Hyper would reject an oversized value at
/// `execute()` time. That is not true for the narrowing paths: `as` wraps to a
/// value that is a perfectly valid encoding of the wrong number, so Hyper
/// accepts it and the `.hyper` file silently holds corrupt data. Those three
/// paths now go through [`narrow_i16`] / [`narrow_i32`] and surface an error to
/// JS instead.
///
/// Float-to-integer coercions keep using `as`, which is *saturating* in Rust
/// rather than wrapping — an out-of-range `f64` clamps to `i32::MIN`/`MAX` and
/// `NaN` becomes 0. That is lossy but bounded, and it remains the documented
/// coercion for this path.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    reason = "caller-selected column coercion: the destination SqlType governs the final encoding and the caller has asserted their value fits; mismatches are surfaced by Hyper at execute() time"
)]
fn add_value_typed(
    chunk: &mut hyperdb_api::InsertChunk,
    val: &InsertValue,
    col_type: Option<hyperdb_api::SqlType>,
) -> std::result::Result<(), hyperdb_api::Error> {
    match val {
        InsertValue::Null => chunk.add_null(),
        InsertValue::Bool(b) => chunk.add_bool(*b),
        InsertValue::String(s) => chunk.add_str(s),
        InsertValue::Bytes(b) => chunk.add_bytes(b),
        InsertValue::I32(n) => match col_type {
            Some(hyperdb_api::SqlType::SmallInt) => chunk.add_i16(narrow_i16(i64::from(*n))?),
            Some(hyperdb_api::SqlType::BigInt) => chunk.add_i64(*n as i64),
            Some(hyperdb_api::SqlType::Double) => chunk.add_f64(*n as f64),
            Some(hyperdb_api::SqlType::Float) => chunk.add_f32(*n as f32),
            _ => chunk.add_i32(*n),
        },
        InsertValue::I64(n) => match col_type {
            Some(hyperdb_api::SqlType::Double) => chunk.add_f64(*n as f64),
            Some(hyperdb_api::SqlType::Float) => chunk.add_f32(*n as f32),
            Some(hyperdb_api::SqlType::Int) => chunk.add_i32(narrow_i32(*n)?),
            Some(hyperdb_api::SqlType::SmallInt) => chunk.add_i16(narrow_i16(*n)?),
            _ => chunk.add_i64(*n),
        },
        InsertValue::F64(n) => match col_type {
            Some(hyperdb_api::SqlType::Int) => chunk.add_i32(*n as i32),
            Some(hyperdb_api::SqlType::SmallInt) => chunk.add_i16(*n as i16),
            Some(hyperdb_api::SqlType::BigInt) => chunk.add_i64(*n as i64),
            Some(hyperdb_api::SqlType::Float) => chunk.add_f32(*n as f32),
            _ => chunk.add_f64(*n),
        },
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "signature kept for API consistency with the trait family that unifies Copy and non-Copy implementers"
)]
/// Parses a JS object `{ 0: number[], 2: number[] }` into a `HashMap<usize, Vec<f64>>`.
fn parse_number_columns(_env: &Env, obj: &Object) -> Result<HashMap<usize, Vec<f64>>> {
    let names = obj.get_property_names()?;
    let len = names.get_named_property::<u32>("length")?;
    let mut map = HashMap::with_capacity(len as usize);

    for i in 0..len {
        let key: napi::Unknown = names.get_element(i)?;
        let key_str = key.coerce_to_string()?.into_utf8()?;
        let col_idx: usize = key_str
            .as_str()?
            .parse()
            .map_err(|_| Error::from_reason("Column index must be a number"))?;

        // JS arrays are indexed by u32 (per the language spec); a column
        // index that does not fit in u32 would be unreachable from JS.
        let col_idx_u32 = u32::try_from(col_idx)
            .map_err(|_| Error::from_reason("Column index exceeds u32 range"))?;
        let arr_val: napi::Unknown = obj.get_element(col_idx_u32)?;
        let arr_obj = arr_val.coerce_to_object()?;
        let arr_len: u32 = arr_obj.get_named_property("length")?;

        let mut values = Vec::with_capacity(arr_len as usize);
        for j in 0..arr_len {
            let v: napi::Unknown = arr_obj.get_element(j)?;
            let n = v.coerce_to_number()?.get_double()?;
            values.push(n);
        }

        map.insert(col_idx, values);
    }

    Ok(map)
}

/// Converts a JS value to our internal `InsertValue` representation.
fn js_value_to_insert_value(val: Unknown) -> Result<InsertValue> {
    match val.get_type()? {
        ValueType::Null | ValueType::Undefined => Ok(InsertValue::Null),
        ValueType::Boolean => {
            // The match arm guarantees a JS boolean; `coerce_to_bool` returns its value.
            let b = val.coerce_to_bool()?;
            Ok(InsertValue::Bool(b))
        }
        ValueType::Number => {
            let n = val.coerce_to_number()?.get_double()?;
            if n.fract() == 0.0 && n >= f64::from(i32::MIN) && n <= f64::from(i32::MAX) {
                // Guard above restricts `n` to `[i32::MIN, i32::MAX]` and
                // integer-valued; the narrowing is a reinterpret of an
                // already-bounded integer.
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "guarded above: `n` is integer-valued and in `[i32::MIN, i32::MAX]`"
                )]
                let narrowed = n as i32;
                Ok(InsertValue::I32(narrowed))
            } else if n.fract() == 0.0
                && {
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "comparing a JS `f64` against the representable `i64` range; the `as f64` on the bounds is exact for i64::MAX (which rounds to 2^63) and exact for i64::MIN, giving the intended inclusive range"
                    )]
                    let in_range = n >= i64::MIN as f64 && n <= i64::MAX as f64;
                    in_range
                }
            {
                // Guard above restricts `n` to `[i64::MIN, i64::MAX]` (modulo
                // the `f64 → i64` rounding at the boundary) and
                // integer-valued; the narrowing is the documented fallback
                // for values outside the i32 range.
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "guarded above: `n` is integer-valued and in `[i64::MIN, i64::MAX]`"
                )]
                let narrowed = n as i64;
                Ok(InsertValue::I64(narrowed))
            } else {
                Ok(InsertValue::F64(n))
            }
        }
        ValueType::String => {
            // SAFETY: `get_type()` returned `ValueType::String`, so the handle is a
            // JS string; `Unknown::cast::<JsString>` wraps it without a further type check.
            let s = unsafe { val.cast::<napi::JsString>() }?
                .into_utf8()?
                .as_str()?
                .to_string();
            Ok(InsertValue::String(s))
        }
        ValueType::Object => {
            // SAFETY: `get_type()` returned `ValueType::Object`. `Unknown::cast::<Buffer>`
            // calls `napi_get_buffer_info`, which returns `Err` for a non-Buffer object,
            // so plain objects fall through to the string-coercion branch.
            if let Ok(buf) = unsafe { val.cast::<Buffer>() } {
                Ok(InsertValue::Bytes(buf.to_vec()))
            } else {
                let s = val.coerce_to_string()?.into_utf8()?.as_str()?.to_string();
                Ok(InsertValue::String(s))
            }
        }
        _ => {
            let s = val.coerce_to_string()?.into_utf8()?.as_str()?.to_string();
            Ok(InsertValue::String(s))
        }
    }
}
