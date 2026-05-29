// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

#![allow(
    clippy::cast_precision_loss,
    reason = "diagnostic rate calculations; bounded chunk sizes"
)]

use napi::bindgen_prelude::*;
use napi_derive::napi;

// =============================================================================
// CellValue - Internal owned representation of a single cell
// =============================================================================

/// Internal representation of a cell value extracted from a query result row.
/// All values are owned so they can outlive the original Row/Rowset.
#[derive(Clone, Debug)]
pub(crate) enum CellValue {
    Null,
    Bool(bool),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    String(String),
    Bytes(Vec<u8>),
    /// Arbitrary-precision decimal, decoded with the column's scale. Stored
    /// as the schema-aware `Numeric` so `getString` can return the exact
    /// decimal text while `getFloat64` returns the (possibly lossy) `f64`.
    Numeric(hyperdb_api::Numeric),
    /// Date as Unix milliseconds (for JS Date interop)
    DateMs(f64),
    /// Timestamp as Unix milliseconds (for JS Date interop)
    TimestampMs(f64),
}

/// Extracts all cell values from a Row using schema information.
pub(crate) fn extract_row(
    row: &hyperdb_api::Row,
    schema: &hyperdb_api::ResultSchema,
) -> Vec<CellValue> {
    let count = row.column_count();
    let mut values = Vec::with_capacity(count);

    for i in 0..count {
        if row.is_null(i) {
            values.push(CellValue::Null);
            continue;
        }

        let sql_type = if i < schema.column_count() {
            schema.column(i).sql_type()
        } else {
            hyperdb_api::SqlType::Text
        };

        let cell = match sql_type {
            hyperdb_api::SqlType::Bool => CellValue::Bool(row.get_bool(i).unwrap_or(false)),
            hyperdb_api::SqlType::SmallInt => CellValue::I16(row.get_i16(i).unwrap_or(0)),
            hyperdb_api::SqlType::Int | hyperdb_api::SqlType::Oid => {
                CellValue::I32(row.get_i32(i).unwrap_or(0))
            }
            hyperdb_api::SqlType::BigInt => CellValue::I64(row.get_i64(i).unwrap_or(0)),
            hyperdb_api::SqlType::Float => CellValue::F32(row.get_f32(i).unwrap_or(0.0)),
            hyperdb_api::SqlType::Double => CellValue::F64(row.get_f64(i).unwrap_or(0.0)),
            hyperdb_api::SqlType::Numeric { .. } => {
                // Schema-aware decode: `get_numeric` reads the column's scale
                // and dispatches on the wire form (8/16-byte, Arrow Decimal).
                // `get_f64` must NOT be used here — it reinterprets the raw
                // unscaled-integer bytes as an IEEE-754 double (garbage/NaN).
                row.get_numeric(i)
                    .map_or(CellValue::Null, CellValue::Numeric)
            }
            hyperdb_api::SqlType::ByteA => CellValue::Bytes(row.get_bytes(i).unwrap_or_default()),
            hyperdb_api::SqlType::Date => {
                match row.get_date(i) {
                    Some(d) => {
                        // Convert Hyper date (days since 2000-01-01) to Unix ms
                        let unix_ms = f64::from(d.days()) * 86_400_000.0 + 946_684_800_000.0;
                        CellValue::DateMs(unix_ms)
                    }
                    None => CellValue::Null,
                }
            }
            hyperdb_api::SqlType::Time => match row.get_time(i) {
                Some(t) => CellValue::String(t.to_string()),
                None => CellValue::Null,
            },
            hyperdb_api::SqlType::Timestamp | hyperdb_api::SqlType::TimestampTz => {
                match row.get_timestamp(i) {
                    Some(ts) => {
                        // Convert Hyper timestamp (µs since 2000-01-01) to Unix ms
                        let unix_ms = ts.microseconds() as f64 / 1000.0 + 946_684_800_000.0;
                        CellValue::TimestampMs(unix_ms)
                    }
                    None => CellValue::Null,
                }
            }
            // Text, Varchar, Char, Json, Interval, Geography, Unsupported → string
            _ => CellValue::String(row.get_string(i).unwrap_or_default()),
        };

        values.push(cell);
    }

    values
}

// =============================================================================
// RowData - JS-visible row object
// =============================================================================

/// A row from a query result.
///
/// Use the typed accessor methods to get column values by index (0-based).
/// Returns `null` for NULL values or type mismatches.
#[napi]
#[derive(Debug)]
pub struct RowData {
    pub(crate) values: Vec<CellValue>,
}

#[napi]
impl RowData {
    /// Returns the number of columns in this row.
    #[napi(getter)]
    pub fn column_count(&self) -> u32 {
        // A row's column count is structurally bounded by Hyper's schema
        // (far below u32::MAX); saturating is a safe diagnostic.
        u32::try_from(self.values.len()).unwrap_or(u32::MAX)
    }

    /// Returns true if the value at the given column index is NULL.
    #[napi]
    pub fn is_null(&self, index: u32) -> bool {
        matches!(
            self.values.get(index as usize),
            Some(CellValue::Null) | None
        )
    }

    /// Gets a boolean value at the given column index.
    #[napi]
    pub fn get_bool(&self, index: u32) -> Option<bool> {
        match self.values.get(index as usize)? {
            CellValue::Bool(v) => Some(*v),
            CellValue::I16(v) => Some(*v != 0),
            CellValue::I32(v) => Some(*v != 0),
            CellValue::I64(v) => Some(*v != 0),
            _ => None,
        }
    }

    /// Gets a 32-bit integer value at the given column index.
    ///
    /// Documented as a narrowing coercion: I64/F32/F64 cells are truncated
    /// to i32. Callers that need lossless access should use `getBigInt()`
    /// (for I64) or `getFloat64()` (for F32/F64).
    #[napi]
    pub fn get_int32(&self, index: u32) -> Option<i32> {
        match self.values.get(index as usize)? {
            CellValue::I16(v) => Some(i32::from(*v)),
            CellValue::I32(v) => Some(*v),
            CellValue::I64(v) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "caller-selected column coercion: I64 cell narrowed to i32 per get_int32 contract"
                )]
                let x = *v as i32;
                Some(x)
            }
            CellValue::F32(v) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "caller-selected column coercion: F32 cell narrowed to i32 per get_int32 contract"
                )]
                let x = *v as i32;
                Some(x)
            }
            CellValue::F64(v) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "caller-selected column coercion: F64 cell narrowed to i32 per get_int32 contract"
                )]
                let x = *v as i32;
                Some(x)
            }
            CellValue::Numeric(n) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "caller-selected column coercion: Numeric cell narrowed to i32 per get_int32 contract"
                )]
                let x = n.to_f64() as i32;
                Some(x)
            }
            _ => None,
        }
    }

    /// Gets a 64-bit integer value at the given column index.
    /// Note: JavaScript numbers lose precision above 2^53. For very large
    /// integers, use `getBigInt()` instead.
    #[napi]
    pub fn get_int64(&self, index: u32) -> Option<i64> {
        match self.values.get(index as usize)? {
            CellValue::I16(v) => Some(i64::from(*v)),
            CellValue::I32(v) => Some(i64::from(*v)),
            CellValue::I64(v) => Some(*v),
            CellValue::F64(v) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "caller-selected column coercion: F64 cell narrowed to i64 per get_int64 contract"
                )]
                let x = *v as i64;
                Some(x)
            }
            CellValue::Numeric(n) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "caller-selected column coercion: Numeric cell narrowed to i64 per get_int64 contract"
                )]
                let x = n.to_f64() as i64;
                Some(x)
            }
            _ => None,
        }
    }

    /// Gets a Date value as a Unix timestamp in milliseconds.
    ///
    /// Pass the result to `new Date(ms)` in JS to get a Date object.
    /// Returns `null` for non-date columns or NULL values.
    #[napi]
    pub fn get_date_ms(&self, index: u32) -> Option<f64> {
        match self.values.get(index as usize)? {
            CellValue::DateMs(ms) => Some(*ms),
            CellValue::I32(v) => {
                // Might be raw days-since-epoch; convert
                Some(f64::from(*v) * 86_400_000.0 + 946_684_800_000.0)
            }
            _ => None,
        }
    }

    /// Gets a Timestamp value as a Unix timestamp in milliseconds.
    ///
    /// Pass the result to `new Date(ms)` in JS to get a Date object.
    /// Works for both TIMESTAMP and TIMESTAMP WITH TIME ZONE columns.
    #[napi]
    pub fn get_timestamp_ms(&self, index: u32) -> Option<f64> {
        match self.values.get(index as usize)? {
            CellValue::TimestampMs(ms) => Some(*ms),
            CellValue::I64(v) => {
                // Might be raw microseconds-since-Hyper-epoch
                Some(*v as f64 / 1000.0 + 946_684_800_000.0)
            }
            _ => None,
        }
    }

    /// Gets a JSON column value as a parsed string (ready for JSON.parse in JS).
    ///
    /// Returns the raw JSON text. Use `JSON.parse(row.getJSON(idx))` in JS.
    #[napi]
    pub fn get_json(&self, index: u32) -> Option<String> {
        match self.values.get(index as usize)? {
            CellValue::String(v) => Some(v.clone()),
            _ => None,
        }
    }

    /// Gets a 64-bit integer as a `BigInt` (no precision loss).
    ///
    /// Unlike `getInt64()` which returns a JS `number` (lossy above 2^53),
    /// this returns a native `BigInt`. For `NUMERIC(p, 0)` columns this
    /// preserves the full 128-bit unscaled value; for `NUMERIC(p, scale>0)`
    /// the cell is not an integer and `null` is returned (use `getString`
    /// for exact decimal text, or `getFloat64` for a lossy numeric value).
    #[napi]
    pub fn get_big_int(&self, index: u32) -> Option<BigInt> {
        match self.values.get(index as usize)? {
            CellValue::I16(v) => Some(BigInt::from(i64::from(*v))),
            CellValue::I32(v) => Some(BigInt::from(i64::from(*v))),
            CellValue::I64(v) => Some(BigInt::from(*v)),
            CellValue::Numeric(n) if n.scale() == 0 => Some(BigInt::from(n.unscaled_value())),
            _ => None,
        }
    }

    /// Gets a double-precision float value at the given column index.
    #[napi]
    pub fn get_float64(&self, index: u32) -> Option<f64> {
        match self.values.get(index as usize)? {
            CellValue::F32(v) => Some(f64::from(*v)),
            CellValue::F64(v) => Some(*v),
            CellValue::I16(v) => Some(f64::from(*v)),
            CellValue::I32(v) => Some(f64::from(*v)),
            CellValue::I64(v) => Some(*v as f64),
            CellValue::Numeric(n) => Some(n.to_f64()),
            _ => None,
        }
    }

    /// Gets a string value at the given column index.
    /// Non-string types are converted to their string representation.
    #[napi]
    pub fn get_string(&self, index: u32) -> Option<String> {
        match self.values.get(index as usize)? {
            CellValue::Null => None,
            CellValue::String(v) => Some(v.clone()),
            CellValue::Bool(v) => Some(v.to_string()),
            CellValue::I16(v) => Some(v.to_string()),
            CellValue::I32(v) => Some(v.to_string()),
            CellValue::I64(v) => Some(v.to_string()),
            CellValue::F32(v) => Some(v.to_string()),
            CellValue::F64(v) => Some(v.to_string()),
            // Exact decimal text (preserves scale and sign); never lossy.
            CellValue::Numeric(n) => Some(n.to_string()),
            CellValue::Bytes(v) => Some(hex::encode(v)),
            CellValue::DateMs(ms) => {
                // Convert back to ISO date string for getString. A Date is
                // bounded to `Date`'s representable range, so the days count
                // fits comfortably in i32; we still clamp to avoid wrapping
                // on a pathological NaN/out-of-range input.
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "days-since-epoch for representable dates fit in i32; Rust saturates f64→i32 at i32::MIN/MAX rather than wrapping"
                )]
                let days = ((*ms - 946_684_800_000.0) / 86_400_000.0) as i32;
                let d = hyperdb_api::Date::from_days(days);
                Some(d.to_string())
            }
            CellValue::TimestampMs(ms) => {
                // Convert back to ISO timestamp string for getString.
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "microseconds-since-epoch for representable timestamps fit in i64; Rust saturates f64→i64 at i64::MIN/MAX rather than wrapping"
                )]
                let micros = ((*ms - 946_684_800_000.0) * 1000.0) as i64;
                let ts = hyperdb_api::Timestamp::from_microseconds(micros);
                Some(ts.to_string())
            }
        }
    }

    /// Gets raw bytes (Buffer) at the given column index.
    #[napi]
    pub fn get_bytes(&self, index: u32) -> Option<Buffer> {
        match self.values.get(index as usize)? {
            CellValue::Bytes(v) => Some(v.clone().into()),
            CellValue::String(v) => Some(v.as_bytes().to_vec().into()),
            _ => None,
        }
    }
}

// Provide hex encoding for bytes → string without adding a dependency.
// We use a simple inline implementation.
mod hex {
    #[expect(
        clippy::format_collect,
        reason = "readable hex/string formatting loop; refactoring to fold! obscures intent"
    )]
    pub(super) fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

// =============================================================================
// ResultSchemaInfo - Column metadata returned to JS
// =============================================================================

/// Metadata about a column in a query result.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ResultColumnInfo {
    /// Column name.
    pub name: String,
    /// SQL type name.
    pub type_name: String,
    /// Column index (0-based).
    pub index: u32,
}
