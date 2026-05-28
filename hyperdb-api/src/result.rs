// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Query result handling with type-safe value access.
//!
//! This module provides types for working with query results:
//! - [`Rowset`] — Streaming result set with memory-efficient chunked iteration
//! - [`RowIterator`] — C++-like iterator for simple row-by-row processing
//! - [`ResultSchema`] — Column metadata (names and types)
//!
//! # Streaming Design
//!
//! Query results are streamed from the server in chunks of up to
//! [`DEFAULT_BINARY_CHUNK_SIZE`] rows (64K). Only one chunk is held in memory
//! at a time, so memory usage is `O(chunk_size)` regardless of total result
//! size — safe for billion-row results.
//!
//! # Iteration Patterns
//!
//! Two patterns are available, both streaming with constant memory:
//!
//! ## Pattern 1: Chunked (`next_chunk()`) — batch processing
//!
//! Best for high-throughput scenarios. Error checking happens once per chunk
//! (~64K rows), and you get direct `Vec<Row>` iteration with good cache
//! locality. Natural for batch operations, vectorized processing, or
//! parallelizing across chunks.
//!
//! ```no_run
//! # use hyperdb_api::{Connection, CreateMode, Result};
//! # fn example(conn: &Connection) -> Result<()> {
//! let mut result = conn.execute_query("SELECT * FROM table")?;
//! while let Some(chunk) = result.next_chunk()? {
//!     for row in &chunk {
//!         let id: Option<i32> = row.get(0);
//!         let value: Option<f64> = row.get(1);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Pattern 2: Iterator (`rows()`) — simple row-by-row
//!
//! Best for simple iteration where you process one row at a time. Each item
//! is `Result<Row>` since chunk fetches can fail, so error checking happens
//! per-row. The extra iterator wrapper adds slight overhead compared to
//! `next_chunk()`.
//!
//! ```no_run
//! # use hyperdb_api::{Connection, Result};
//! # fn example(conn: &Connection) -> Result<()> {
//! let result = conn.execute_query("SELECT * FROM table")?;
//! for row in result.rows() {
//!     let row = row?;  // Handle potential errors
//!     let id: Option<i32> = row.get(0);
//!     let value: Option<f64> = row.get(1);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! **When to use which:**
//! - `rows()` — simple iteration, one row at a time, small overhead acceptable
//! - `next_chunk()` — maximum performance, large result sets, batch operations
//!
//! # Type Coercion
//!
//! The generic `row.get::<T>()` method supports automatic widening coercion:
//!
//! | Request Type | Coerces From |
//! |---|---|
//! | `i32` | `i16` |
//! | `i64` | `i32`, `i16` |
//! | `f64` | `f32` |
//!
//! Direct accessors (`row.get_i32()`, `row.get_f64()`) skip coercion for
//! slightly better performance when the exact type is known.

use std::sync::Arc;

use arrow::array::Array;
use arrow::record_batch::RecordBatch;
use hyperdb_api_core::client::QueryStream;
use hyperdb_api_core::client::StreamRow;
use hyperdb_api_core::types::SqlType;

use crate::arrow_result::{ArrowRowset, FromArrowValue};
use crate::error::Result;

/// Default chunk size for streaming queries (64K rows).
pub(crate) const DEFAULT_BINARY_CHUNK_SIZE: usize = 65536;

// =============================================================================
// Row - Unified row type for both TCP and gRPC
// =============================================================================

/// A row from a query result, providing typed value access.
///
/// This type abstracts over the underlying transport (TCP or gRPC),
/// providing a consistent API for accessing column values regardless
/// of how the data was retrieved.
///
/// # Example
///
/// ```no_run
/// # use hyperdb_api::Result;
/// # fn example(result: hyperdb_api::Rowset) -> Result<()> {
/// for row in result.rows() {
///     let row = row?;
///     let id: Option<i32> = row.get(0);
///     let name: Option<String> = row.get(1);
///     // Or use direct accessors
///     let value = row.get_f64(2);
/// }
/// # Ok(())
/// # }
/// ```
pub struct Row {
    inner: RowInner,
    /// Shared schema reference for the parent rowset. Every row
    /// produced by [`Rowset::next_chunk`] carries this (cloned cheaply
    /// from an `Arc`) so that metadata-dependent decoders like
    /// [`Self::get_numeric`] can look up `SqlType` per column without
    /// the caller plumbing scale through manually. `None` only in the
    /// unusual case a row is constructed outside `next_chunk` (no such
    /// path exists in-tree today; the field is `Option` so future
    /// schemas-unavailable paths remain compilable).
    schema: Option<Arc<ResultSchema>>,
}

impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("has_schema", &self.schema.is_some())
            .finish_non_exhaustive()
    }
}

/// Internal per-transport backing for a [`Row`]. Not public: all
/// consumer-visible API goes through `Row`'s methods, which dispatch
/// on this enum internally.
enum RowInner {
    /// Row from TCP transport (`StreamRow`).
    Tcp(StreamRow),
    /// Row from gRPC transport (Arrow-backed).
    Arrow {
        /// The record batch containing this row's data.
        batch: Arc<RecordBatch>,
        /// Index of this row within the batch.
        row_index: usize,
    },
}

impl Row {
    /// Construct a TCP-backed row with an attached schema reference.
    #[inline]
    pub(crate) fn from_tcp(row: StreamRow, schema: Option<Arc<ResultSchema>>) -> Self {
        Row {
            inner: RowInner::Tcp(row),
            schema,
        }
    }

    /// Construct an Arrow-backed row with an attached schema reference.
    #[inline]
    pub(crate) fn from_arrow(
        batch: Arc<RecordBatch>,
        row_index: usize,
        schema: Option<Arc<ResultSchema>>,
    ) -> Self {
        Row {
            inner: RowInner::Arrow { batch, row_index },
            schema,
        }
    }

    /// Returns the schema this row belongs to, if attached.
    ///
    /// Every row produced by [`Rowset::next_chunk`] has a schema
    /// attached — so this returns `Some` for any row obtained through
    /// the public API.
    #[inline]
    pub fn schema(&self) -> Option<&ResultSchema> {
        self.schema.as_deref()
    }

    /// Returns the `SqlType` of the column at the given index, if the
    /// schema is attached and the index is in bounds.
    ///
    /// Useful for metadata-dependent decoders like [`Self::get_numeric`]
    /// that need per-column precision and scale. Most callers reach for
    /// [`Self::get`] / [`Self::try_get`] instead, which handle this
    /// lookup internally via the [`RowValue`] trait.
    #[inline]
    pub fn sql_type(&self, idx: usize) -> Option<SqlType> {
        let schema = self.schema.as_deref()?;
        if idx < schema.column_count() {
            Some(schema.column(idx).sql_type())
        } else {
            None
        }
    }

    /// Gets a typed value at the given column index.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::Row;
    /// # fn example(row: &Row) {
    /// let id: Option<i32> = row.get(0);
    /// let name: Option<String> = row.get(1);
    /// # }
    /// ```
    #[inline]
    pub fn get<T: RowValue>(&self, idx: usize) -> Option<T> {
        T::from_row(self, idx)
    }

    /// Gets a typed value at the given column index, returning a `Result`
    /// with a descriptive error on failure.
    ///
    /// Use this in [`FromRow`] implementations for better error messages
    /// than bare `row.get(idx).ok_or(...)`.
    ///
    /// # Example
    ///
    /// Most callers should reach for [`crate::FromRow`] +
    /// [`crate::RowAccessor`] for typed mapping. `try_get` is the
    /// underlying positional building block; useful when you need
    /// indexed access from a hand-rolled loop.
    ///
    /// ```no_run
    /// # use hyperdb_api::{Row, Result};
    /// # fn read(row: &Row) -> Result<(i32, String)> {
    /// let id: i32 = row.try_get(0, "id")?;
    /// let name: String = row.try_get(1, "name")?;
    /// # Ok((id, name))
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`crate::Error::Conversion`] if `idx` is out of bounds for the row's
    ///   column count.
    /// - Returns [`crate::Error::Conversion`] if the cell is SQL `NULL` or its value
    ///   cannot be decoded as `T`.
    pub fn try_get<T: RowValue>(&self, idx: usize, column_name: &str) -> crate::error::Result<T> {
        if idx >= self.column_count() {
            return Err(crate::error::Error::conversion(format!(
                "Column index {} ({:?}) out of bounds — row has {} columns",
                idx,
                column_name,
                self.column_count(),
            )));
        }
        self.get::<T>(idx).ok_or_else(|| {
            crate::error::Error::conversion(format!(
                "Column {idx} ({column_name:?}) is NULL or has incompatible type",
            ))
        })
    }

    /// Looks up a column by name and returns its value as `T`.
    ///
    /// Convenient for hand-coded paths that aren't using
    /// [`FromRow`]. The lookup is a linear scan over
    /// [`ResultSchema::column_index`]; for hot paths (many rows × many
    /// fields), prefer
    /// [`fetch_one_as`](crate::Connection::fetch_one_as) /
    /// [`fetch_all_as`](crate::Connection::fetch_all_as), which build
    /// a cached column-name → index lookup once per query and hand
    /// every `FromRow` impl a [`RowAccessor`](crate::RowAccessor) that
    /// reuses it.
    ///
    /// # Errors
    ///
    /// - [`crate::Error::Column`] with [`crate::ColumnErrorKind::Missing`]
    ///   if no column with `name` exists in the row's schema (or the
    ///   row has no schema attached).
    /// - [`crate::Error::Conversion`] if the cell is `NULL` or cannot
    ///   be decoded as `T`. (Inherited from [`Self::try_get`].)
    pub fn get_by_name<T: RowValue>(&self, name: &str) -> crate::error::Result<T> {
        let idx = self
            .schema()
            .and_then(|s| s.column_index(name))
            .ok_or_else(|| {
                crate::error::Error::column(name, crate::error::ColumnErrorKind::Missing)
            })?;
        self.try_get(idx, name)
    }

    /// Returns an Arrow column reference, or `None` if the index is out of bounds.
    ///
    /// This is a safe wrapper around `batch.column(idx)` that avoids panicking.
    #[inline]
    fn arrow_column(batch: &RecordBatch, idx: usize) -> Option<&Arc<dyn Array>> {
        if idx < batch.num_columns() {
            Some(batch.column(idx))
        } else {
            None
        }
    }

    /// Gets an i16 value at the given column index.
    #[inline]
    pub fn get_i16(&self, idx: usize) -> Option<i16> {
        match &self.inner {
            RowInner::Tcp(row) => row.get_i16(idx),
            RowInner::Arrow { batch, row_index } => {
                i16::from_arrow_column(Self::arrow_column(batch, idx)?, *row_index)
            }
        }
    }

    /// Gets an i32 value at the given column index.
    #[inline]
    pub fn get_i32(&self, idx: usize) -> Option<i32> {
        match &self.inner {
            RowInner::Tcp(row) => row.get_i32(idx),
            RowInner::Arrow { batch, row_index } => {
                i32::from_arrow_column(Self::arrow_column(batch, idx)?, *row_index)
            }
        }
    }

    /// Gets an i64 value at the given column index.
    #[inline]
    pub fn get_i64(&self, idx: usize) -> Option<i64> {
        match &self.inner {
            RowInner::Tcp(row) => row.get_i64(idx),
            RowInner::Arrow { batch, row_index } => {
                i64::from_arrow_column(Self::arrow_column(batch, idx)?, *row_index)
            }
        }
    }

    /// Gets an f32 value at the given column index.
    #[inline]
    pub fn get_f32(&self, idx: usize) -> Option<f32> {
        match &self.inner {
            RowInner::Tcp(row) => row.get_f32(idx),
            RowInner::Arrow { batch, row_index } => {
                f32::from_arrow_column(Self::arrow_column(batch, idx)?, *row_index)
            }
        }
    }

    /// Gets an f64 value at the given column index.
    #[inline]
    pub fn get_f64(&self, idx: usize) -> Option<f64> {
        match &self.inner {
            RowInner::Tcp(row) => row.get_f64(idx),
            RowInner::Arrow { batch, row_index } => {
                f64::from_arrow_column(Self::arrow_column(batch, idx)?, *row_index)
            }
        }
    }

    /// Gets a bool value at the given column index.
    #[inline]
    pub fn get_bool(&self, idx: usize) -> Option<bool> {
        match &self.inner {
            RowInner::Tcp(row) => row.get_bool(idx),
            RowInner::Arrow { batch, row_index } => {
                bool::from_arrow_column(Self::arrow_column(batch, idx)?, *row_index)
            }
        }
    }

    /// Gets a String value at the given column index.
    #[inline]
    pub fn get_string(&self, idx: usize) -> Option<String> {
        match &self.inner {
            RowInner::Tcp(row) => row.get_string(idx),
            RowInner::Arrow { batch, row_index } => {
                String::from_arrow_column(Self::arrow_column(batch, idx)?, *row_index)
            }
        }
    }

    /// Checks if the value at the given column is null.
    #[inline]
    pub fn is_null(&self, idx: usize) -> bool {
        match &self.inner {
            RowInner::Tcp(row) => row.is_null(idx),
            RowInner::Arrow { batch, row_index } => match Self::arrow_column(batch, idx) {
                Some(col) => col.is_null(*row_index),
                None => true,
            },
        }
    }

    /// Returns the number of columns in this row.
    #[inline]
    pub fn column_count(&self) -> usize {
        match &self.inner {
            RowInner::Tcp(row) => row.column_count(),
            RowInner::Arrow { batch, .. } => batch.num_columns(),
        }
    }

    /// Gets raw bytes at the given column index.
    ///
    /// For TCP rows, returns the raw binary data. For Arrow rows, this method
    /// is not available and returns None.
    #[inline]
    pub fn get_bytes(&self, idx: usize) -> Option<Vec<u8>> {
        match &self.inner {
            RowInner::Tcp(row) => row.get_bytes(idx).map(<[u8]>::to_vec),
            RowInner::Arrow { batch, row_index } => {
                Vec::<u8>::from_arrow_column(Self::arrow_column(batch, idx)?, *row_index)
            }
        }
    }

    /// Gets a Date value at the given column index.
    #[inline]
    pub fn get_date(&self, idx: usize) -> Option<hyperdb_api_core::types::Date> {
        match &self.inner {
            RowInner::Tcp(row) => row.get(idx),
            RowInner::Arrow { batch, row_index } => {
                // Arrow Date32 is days since Unix epoch (1970-01-01)
                // Hyper Date is days since Hyper epoch (2000-01-01)
                use arrow::array::Date32Array;
                let col = Self::arrow_column(batch, idx)?;
                let arr = col.as_any().downcast_ref::<Date32Array>()?;
                if arr.is_null(*row_index) {
                    return None;
                }
                let unix_days = arr.value(*row_index);
                // Convert from Unix epoch to Hyper epoch (diff is 10957 days)
                let hyper_days = unix_days - 10957;
                Some(hyperdb_api_core::types::Date::from_days(hyper_days))
            }
        }
    }

    /// Gets a Time value at the given column index.
    #[inline]
    pub fn get_time(&self, idx: usize) -> Option<hyperdb_api_core::types::Time> {
        match &self.inner {
            RowInner::Tcp(row) => row.get(idx),
            RowInner::Arrow { batch, row_index } => {
                // Arrow Time64 is microseconds since midnight
                use arrow::array::Time64MicrosecondArray;
                let col = Self::arrow_column(batch, idx)?;
                let arr = col.as_any().downcast_ref::<Time64MicrosecondArray>()?;
                if arr.is_null(*row_index) {
                    return None;
                }
                let micros = u64::try_from(arr.value(*row_index)).ok()?;
                Some(hyperdb_api_core::types::Time::from_microseconds(micros))
            }
        }
    }

    /// Gets a Timestamp value at the given column index.
    #[inline]
    pub fn get_timestamp(&self, idx: usize) -> Option<hyperdb_api_core::types::Timestamp> {
        match &self.inner {
            RowInner::Tcp(row) => row.get(idx),
            RowInner::Arrow { batch, row_index } => {
                // Arrow Timestamp is microseconds since Unix epoch
                // Hyper Timestamp is microseconds since Hyper epoch (2000-01-01)
                use arrow::array::TimestampMicrosecondArray;
                let col = Self::arrow_column(batch, idx)?;
                let arr = col.as_any().downcast_ref::<TimestampMicrosecondArray>()?;
                if arr.is_null(*row_index) {
                    return None;
                }
                let unix_micros = arr.value(*row_index);
                // Convert from Unix epoch to Hyper epoch
                // 2000-01-01 is 946684800 seconds after 1970-01-01
                let hyper_micros = unix_micros - 946_684_800_000_000;
                Some(hyperdb_api_core::types::Timestamp::from_microseconds(
                    hyper_micros,
                ))
            }
        }
    }

    /// Gets an `OffsetTimestamp` (TIMESTAMP WITH TIME ZONE) value at the given column index.
    #[inline]
    pub fn get_offset_timestamp(
        &self,
        idx: usize,
    ) -> Option<hyperdb_api_core::types::OffsetTimestamp> {
        match &self.inner {
            RowInner::Tcp(row) => row.get(idx),
            RowInner::Arrow { batch, row_index } => {
                // Arrow TimestampTz is microseconds since Unix epoch with timezone
                use arrow::array::TimestampMicrosecondArray;
                let col = Self::arrow_column(batch, idx)?;
                let arr = col.as_any().downcast_ref::<TimestampMicrosecondArray>()?;
                if arr.is_null(*row_index) {
                    return None;
                }
                let unix_micros = arr.value(*row_index);
                let hyper_micros = unix_micros - 946_684_800_000_000;
                let ts = hyperdb_api_core::types::Timestamp::from_microseconds(hyper_micros);
                Some(hyperdb_api_core::types::OffsetTimestamp::new(ts, 0))
            }
        }
    }

    /// Gets an Interval value at the given column index.
    #[inline]
    pub fn get_interval(&self, idx: usize) -> Option<hyperdb_api_core::types::Interval> {
        match &self.inner {
            RowInner::Tcp(row) => row.get(idx),
            RowInner::Arrow { batch, row_index } => {
                // Arrow MonthDayNano interval → Hyper Interval
                use arrow::array::IntervalMonthDayNanoArray;
                let col = Self::arrow_column(batch, idx)?;
                let arr = col.as_any().downcast_ref::<IntervalMonthDayNanoArray>()?;
                if arr.is_null(*row_index) {
                    return None;
                }
                let v = arr.value(*row_index);
                let micros = v.nanoseconds / 1000;
                Some(hyperdb_api_core::types::Interval::new(
                    v.months, v.days, micros,
                ))
            }
        }
    }

    /// Gets a `NUMERIC` value at the given column index.
    ///
    /// This is the metadata-aware variant of [`Self::get_bytes`] +
    /// [`hyperdb_api_core::types::Numeric::from_binary_with_scale`]: it looks up
    /// the column's `SqlType::Numeric { scale, .. }` from the attached
    /// schema and decodes the wire bytes with that scale, handling
    /// both of Hyper's NUMERIC wire forms transparently:
    ///
    /// - **8 bytes** (i64) when the column's declared precision ≤ 18
    ///   (Hyper's `Type::Numeric`). This is what aggregates like
    ///   `AVG(INTEGER)` return as `Numeric(16, 6)`.
    /// - **16 bytes** (i128) when declared precision > 18
    ///   (Hyper's `Type::BigNumeric`).
    ///
    /// Returns `None` if any of the following are true: the value is
    /// NULL, the schema isn't attached (which never happens for rows
    /// obtained through [`Rowset::next_chunk`]), the column at `idx`
    /// isn't `NUMERIC`, or the bytes can't be decoded.
    ///
    /// For non-TCP (Arrow/gRPC) rows, this path falls back to reading
    /// the Arrow-native `Decimal128` / `Decimal256` columns; the scale
    /// lives in the Arrow type descriptor in that case.
    pub fn get_numeric(&self, idx: usize) -> Option<hyperdb_api_core::types::Numeric> {
        match &self.inner {
            RowInner::Tcp(_) => {
                // TCP: decode raw bytes with scale from the schema.
                //
                // `SqlType::Numeric::scale` is `u32` and Hyper's own
                // `NUMERIC(p, s)` caps at `p ≤ 38` (per
                // `hyper/rts/type/Type.hpp`), so any legitimate scale
                // fits easily in `u8`. But `scale as u8` silently
                // truncates the high bits for values > 255, and a
                // malformed server response or a bug in typemod
                // parsing could deliver such a value — at which point
                // we'd produce a `Numeric` with the wrong (truncated)
                // scale and no error signal. `u8::try_from` returns
                // `Err` for out-of-range, `?` propagates `None`, and
                // the caller gets a clean "no value" instead of
                // silent corruption. Symmetric with the Arrow
                // negative-scale guard a few lines below.
                let scale: u8 = match self.sql_type(idx)? {
                    SqlType::Numeric { scale, .. } => u8::try_from(scale).ok()?,
                    _ => return None,
                };
                let bytes = self.get_bytes(idx)?;
                hyperdb_api_core::types::Numeric::from_binary_with_scale(&bytes, scale).ok()
            }
            RowInner::Arrow { batch, row_index } => {
                use arrow::array::{Decimal128Array, Decimal256Array};
                use arrow::datatypes::DataType as ArrowType;
                let col = Self::arrow_column(batch, idx)?;
                // Arrow stores decimal precision/scale in the type
                // descriptor itself, so there's no separate schema
                // lookup needed on this path.
                //
                // Note: Arrow's decimal scale is `i8` and can legally
                // be negative (negative scale = "value is multiplied
                // by 10^abs(scale)", e.g. scale=-2 on raw=5 renders
                // as 500). Hyper's `Numeric` uses `u8` scale and has
                // no representation for the negative-scale
                // multiplier. Rather than silently dropping the
                // multiplier (which would make raw=5 display as 5
                // instead of 500), we surface it as "no value" via
                // `try_into` + `?`. Negative-scale decimals don't
                // originate from Hyper's own gRPC encoder — but
                // `Row` can be fed from externally-loaded Arrow
                // files, so defensive handling costs nothing and
                // prevents a silent-corruption failure mode.
                match col.data_type() {
                    ArrowType::Decimal128(_precision, scale) => {
                        let scale_u8: u8 = (*scale).try_into().ok()?;
                        let arr = col.as_any().downcast_ref::<Decimal128Array>()?;
                        if arr.is_null(*row_index) {
                            return None;
                        }
                        let raw = arr.value(*row_index); // i128
                        Some(hyperdb_api_core::types::Numeric::new(raw, scale_u8))
                    }
                    ArrowType::Decimal256(_precision, scale) => {
                        // i256 from Arrow; Hyper NUMERIC caps at i128
                        // (precision ≤ 38). Narrow to i128; this is
                        // lossless for any value Hyper would actually
                        // produce. Values outside that range are a
                        // server-side contract violation.
                        let scale_u8: u8 = (*scale).try_into().ok()?;
                        let arr = col.as_any().downcast_ref::<Decimal256Array>()?;
                        if arr.is_null(*row_index) {
                            return None;
                        }
                        let raw = arr.value(*row_index);
                        let as_i128: i128 = raw.to_i128()?;
                        Some(hyperdb_api_core::types::Numeric::new(as_i128, scale_u8))
                    }
                    _ => None,
                }
            }
        }
    }
}

/// Trait for types that can be extracted from a Row.
pub trait RowValue: Sized {
    /// Extract a value from a Row at the given column index.
    fn from_row(row: &Row, idx: usize) -> Option<Self>;
}

impl RowValue for i16 {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_i16(idx)
    }
}

impl RowValue for i32 {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_i32(idx).or_else(|| row.get_i16(idx).map(i32::from))
    }
}

impl RowValue for i64 {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_i64(idx)
            .or_else(|| row.get_i32(idx).map(i64::from))
            .or_else(|| row.get_i16(idx).map(i64::from))
    }
}

impl RowValue for f32 {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_f32(idx)
    }
}

impl RowValue for f64 {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_f64(idx).or_else(|| row.get_f32(idx).map(f64::from))
    }
}

impl RowValue for bool {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_bool(idx)
    }
}

impl RowValue for String {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_string(idx)
    }
}

impl RowValue for Vec<u8> {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_bytes(idx)
    }
}

impl RowValue for hyperdb_api_core::types::Date {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_date(idx)
    }
}

impl RowValue for hyperdb_api_core::types::Time {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_time(idx)
    }
}

impl RowValue for hyperdb_api_core::types::Timestamp {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_timestamp(idx)
    }
}

impl RowValue for hyperdb_api_core::types::OffsetTimestamp {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_offset_timestamp(idx)
    }
}

impl RowValue for hyperdb_api_core::types::Interval {
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_interval(idx)
    }
}

impl RowValue for hyperdb_api_core::types::Numeric {
    /// Unlike every other `RowValue` impl, `Numeric` decode requires
    /// per-column metadata (scale + wire-form width) that lives on the
    /// row's attached `ResultSchema`. [`Row::get_numeric`] does the
    /// lookup; this impl delegates there so generic `row.get::<Numeric>()`
    /// / `row.try_get::<Numeric>(idx, "name")` call sites work the same
    /// as every other type.
    #[inline]
    fn from_row(row: &Row, idx: usize) -> Option<Self> {
        row.get_numeric(idx)
    }
}

// =============================================================================
// FromRow - Struct mapping trait
// =============================================================================

/// Trait for types that can be constructed from a database row.
///
/// Used by [`Connection::fetch_one_as`](crate::Connection::fetch_one_as)
/// and [`Connection::fetch_all_as`](crate::Connection::fetch_all_as)
/// to map query results into typed structs. Implementations receive
/// a [`RowAccessor`](crate::RowAccessor), which provides name-based
/// access via a column-name → index lookup built once per query.
///
/// # Recommended: derive
///
/// In most cases the `#[derive(FromRow)]` macro handles the mapping
/// for you — match struct field names to column names automatically,
/// with `#[hyperdb(rename = "...")]` for cases where they differ:
///
/// ```ignore
/// use hyperdb_api::FromRow;
///
/// #[derive(FromRow)]
/// struct User {
///     id: i32,
///     name: String,
///     #[hyperdb(rename = "email_address")]
///     email: Option<String>,
/// }
/// ```
///
/// # Hand-written impl
///
/// For custom mapping logic (computed fields, multi-column composition,
/// etc.) implement the trait directly:
///
/// ```no_run
/// use hyperdb_api::{FromRow, RowAccessor, Result};
///
/// struct User { id: i32, name: String, active: bool }
///
/// impl FromRow for User {
///     fn from_row(row: RowAccessor<'_>) -> Result<Self> {
///         Ok(User {
///             id: row.get("id")?,
///             name: row.get("name")?,
///             active: row.get("active")?,
///         })
///     }
/// }
/// ```
///
/// For ad-hoc tuple destructuring of small results, use
/// [`Row::get`](crate::Row::get) directly — there are no blanket
/// tuple `FromRow` impls. Define a struct with `#[derive(FromRow)]`
/// for typed access in `fetch_*_as`.
pub trait FromRow: Sized {
    /// Constructs an instance from a database row.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`](crate::Error) — typically
    /// [`crate::Error::Column`] — when a required column is missing,
    /// SQL `NULL`, or cannot be decoded as the expected type.
    /// Implementations decide the exact failure shape.
    fn from_row(row: crate::RowAccessor<'_>) -> crate::error::Result<Self>;
}

// =============================================================================
// ResultSchema and ResultColumn
// =============================================================================

/// Metadata about a column in a result schema.
#[derive(Debug, Clone)]
pub struct ResultColumn {
    /// The column name.
    name: String,
    /// The SQL type of the column.
    sql_type: SqlType,
    /// The column index (0-based).
    index: usize,
}

impl ResultColumn {
    /// Creates a new result column.
    pub fn new(name: impl Into<String>, sql_type: SqlType, index: usize) -> Self {
        ResultColumn {
            name: name.into(),
            sql_type,
            index,
        }
    }

    /// Returns the column name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the SQL type of the column.
    #[must_use]
    pub fn sql_type(&self) -> SqlType {
        self.sql_type
    }

    /// Returns the column index (0-based).
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }
}

/// Schema information for a query result.
///
/// Provides metadata about the columns returned by a query, including
/// column names and types.
#[derive(Debug, Clone, Default)]
pub struct ResultSchema {
    columns: Vec<ResultColumn>,
}

impl ResultSchema {
    /// Creates a new empty result schema.
    #[must_use]
    pub fn new() -> Self {
        ResultSchema {
            columns: Vec::new(),
        }
    }

    /// Creates a result schema from column definitions.
    #[must_use]
    pub fn from_columns(columns: Vec<ResultColumn>) -> Self {
        ResultSchema { columns }
    }

    /// Adds a column to the schema.
    pub fn add_column(&mut self, name: impl Into<String>, sql_type: SqlType) {
        let index = self.columns.len();
        self.columns.push(ResultColumn::new(name, sql_type, index));
    }

    /// Returns the number of columns.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Returns all columns.
    #[must_use]
    pub fn columns(&self) -> &[ResultColumn] {
        &self.columns
    }

    /// Returns the column at the given index.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds.
    #[must_use]
    pub fn column(&self, index: usize) -> &ResultColumn {
        &self.columns[index]
    }

    /// Returns the column with the given name, if it exists.
    #[must_use]
    pub fn column_by_name(&self, name: &str) -> Option<&ResultColumn> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Returns the index of the column with the given name, if it exists.
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

// =============================================================================
// Rowset (Streaming)
// =============================================================================

/// A streaming result set from a SQL query.
///
/// `Rowset` provides memory-efficient streaming access to query results.
/// Results are fetched on-demand in chunks, keeping memory usage constant
/// regardless of result set size. This makes it safe for any result size,
/// from a single row to billions of rows.
///
/// # Example
///
/// ```no_run
/// # use hyperdb_api::{Connection, Result};
/// # fn example(conn: &Connection) -> Result<()> {
/// let mut result = conn.execute_query("SELECT * FROM big_table")?;
/// while let Some(chunk) = result.next_chunk()? {
///     for row in &chunk {
///         // Generic typed access (like C++ row.get<T>())
///         let id: Option<i32> = row.get(0);
///         let value: Option<f64> = row.get(1);
///
///         // Or direct accessors for performance
///         let id = row.get_i32(0);
///         let value = row.get_f64(1);
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Memory Behavior
///
/// - Only one chunk is held in memory at a time
/// - Default chunk size is 64K rows (~few MB depending on row width)
/// - Memory usage is `O(chunk_size)`, not `O(total_rows)`
/// - Safe for billion-row results
pub struct Rowset<'conn> {
    inner: RowsetInner<'conn>,
    /// Cached schema for this rowset, built lazily the first time
    /// [`Self::next_chunk`] produces a non-empty chunk (TCP path — at
    /// which point the `RowDescription` message has been observed) or
    /// on first Arrow chunk (gRPC path). Stored as `Arc` so each row
    /// produced by `next_chunk` gets a cheap ref-count clone — that's
    /// how metadata-dependent decoders like [`Row::get_numeric`] reach
    /// the column's `SqlType` without the caller plumbing scale
    /// through manually.
    schema_cache: Option<Arc<ResultSchema>>,
    /// For one-shot prepared statements (the internal
    /// [`crate::Connection::query_params`] path), hold the statement
    /// handle here so its `Drop`-time `close_statement` fires *after*
    /// the rowset releases its connection lock. Dropping the statement
    /// before the rowset would deadlock because the inner stream owns
    /// the connection's `MutexGuard`.
    _statement_guard: Option<hyperdb_api_core::client::OwnedPreparedStatement>,
}

impl std::fmt::Debug for Rowset<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rowset")
            .field("has_schema_cache", &self.schema_cache.is_some())
            .finish_non_exhaustive()
    }
}

/// Internal enum to hold either TCP stream or Arrow data.
enum RowsetInner<'conn> {
    /// TCP streaming result (uses `QueryStream`).
    Tcp(QueryStream<'conn>),
    /// Arrow-based result from gRPC (all data loaded).
    Arrow(ArrowRowset),
    /// TCP streaming result from a prepared-statement execute.
    Prepared(hyperdb_api_core::client::PreparedQueryStream<'conn>),
}

impl<'conn> Rowset<'conn> {
    /// Creates a new Rowset from a `QueryStream` (TCP).
    pub(crate) fn new(stream: QueryStream<'conn>) -> Self {
        Rowset {
            inner: RowsetInner::Tcp(stream),
            schema_cache: None,
            _statement_guard: None,
        }
    }

    /// Creates a new Rowset from Arrow IPC data (gRPC).
    pub(crate) fn from_arrow(arrow_rowset: ArrowRowset) -> Self {
        Rowset {
            inner: RowsetInner::Arrow(arrow_rowset),
            schema_cache: None,
            _statement_guard: None,
        }
    }

    /// Creates a new Rowset from a prepared-statement streaming result.
    pub(crate) fn from_prepared(
        stream: hyperdb_api_core::client::PreparedQueryStream<'conn>,
    ) -> Self {
        Rowset {
            inner: RowsetInner::Prepared(stream),
            schema_cache: None,
            _statement_guard: None,
        }
    }

    #[expect(
        clippy::used_underscore_binding,
        reason = "underscore-prefixed parameter retained for trait-method signature compatibility"
    )]
    /// Attaches a `OwnedPreparedStatement` that should be dropped
    /// **after** this rowset is consumed. Used by the one-shot
    /// prepare+execute path inside
    /// [`crate::Connection::query_params`] so the statement's
    /// Drop-time close doesn't deadlock on the rowset's still-held
    /// connection lock.
    pub(crate) fn with_statement_guard(
        mut self,
        statement: hyperdb_api_core::client::OwnedPreparedStatement,
    ) -> Self {
        self._statement_guard = Some(statement);
        self
    }

    /// Returns the schema (column metadata) for the result set.
    ///
    /// For TCP connections, the schema is captured from the `RowDescription` message
    /// after the first chunk is read. For gRPC connections, the schema is available
    /// immediately from the Arrow data.
    ///
    /// Returns `None` if no data has been read yet (TCP only).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let mut result = conn.execute_query("SELECT id, name FROM users")?;
    /// // Read first chunk to capture schema (TCP) or get it immediately (gRPC)
    /// let _ = result.next_chunk()?;
    /// if let Some(schema) = result.schema() {
    ///     for col in schema.columns() {
    ///         println!("Column: {} ({})", col.name(), col.sql_type());
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn schema(&self) -> Option<ResultSchema> {
        // Fast path: cache already populated by a previous call or by
        // `next_chunk`. Clone from the Arc so external callers get an
        // owned value independent of internal lifetimes.
        if let Some(ref cached) = self.schema_cache {
            return Some((**cached).clone());
        }
        // Slow path: schema hasn't been materialized yet. Build it from
        // the transport without populating the cache — `schema()` takes
        // `&self`, so mutation isn't possible here. `next_chunk` does
        // the caching pass for the row-construction hot path; if a
        // caller really only wants the schema and never touches rows,
        // they pay one build per call but this is rarely the pattern.
        self.build_schema()
    }

    /// Compute the current schema without populating the cache.
    ///
    /// Pulls column metadata from the underlying transport and
    /// constructs a fresh `ResultSchema`. TCP builds `SqlType` via
    /// [`SqlType::from_oid_and_modifier`] so
    /// `NUMERIC(precision, scale)` and `VARCHAR(n)` recover their
    /// declared parameters from the `RowDescription` `atttypmod`
    /// field — dropping the modifier (which bare
    /// [`SqlType::from_oid`] does) silently turns every `NUMERIC`
    /// into `(precision: 0, scale: 0)` and corrupts decimal decodes
    /// downstream. Arrow comes pre-typed via
    /// `arrow_type_to_sql_type`.
    fn build_schema(&self) -> Option<ResultSchema> {
        match &self.inner {
            RowsetInner::Tcp(stream) => stream.schema().map(|cols| {
                let columns = cols
                    .iter()
                    .enumerate()
                    .map(|(idx, col)| {
                        let sql_type =
                            SqlType::from_oid_and_modifier(col.type_oid().0, col.type_modifier());
                        ResultColumn::new(col.name(), sql_type, idx)
                    })
                    .collect();
                ResultSchema::from_columns(columns)
            }),
            RowsetInner::Arrow(arrow) => {
                let schema = arrow.schema();
                let columns = schema
                    .fields()
                    .iter()
                    .enumerate()
                    .map(|(idx, field)| {
                        ResultColumn::new(
                            field.name(),
                            crate::arrow_result::arrow_type_to_sql_type(field.data_type()),
                            idx,
                        )
                    })
                    .collect();
                Some(ResultSchema::from_columns(columns))
            }
            // Prepared statements: schema was captured at prepare time,
            // so it is always available immediately.
            RowsetInner::Prepared(stream) => {
                let cols = stream.schema();
                let columns = cols
                    .iter()
                    .enumerate()
                    .map(|(idx, col)| {
                        let sql_type =
                            SqlType::from_oid_and_modifier(col.type_oid().0, col.type_modifier());
                        ResultColumn::new(col.name(), sql_type, idx)
                    })
                    .collect();
                Some(ResultSchema::from_columns(columns))
            }
        }
    }

    /// Populate `schema_cache` if not yet set, then return an `Arc`
    /// clone of the cached schema for row construction. Called by
    /// `next_chunk` so every row produced gets a cheap schema
    /// reference without re-building the `ResultSchema` per chunk.
    fn cached_schema_arc(&mut self) -> Option<Arc<ResultSchema>> {
        if self.schema_cache.is_none() {
            if let Some(schema) = self.build_schema() {
                self.schema_cache = Some(Arc::new(schema));
            }
        }
        self.schema_cache.clone()
    }

    /// Returns the next chunk of rows from the result set.
    ///
    /// Each chunk contains up to `chunk_size` rows (default 64K).
    /// Returns `Ok(None)` when all rows have been consumed.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Rowset, Result};
    /// # fn example(mut result: Rowset) -> Result<()> {
    /// while let Some(chunk) = result.next_chunk()? {
    ///     for row in &chunk {
    ///         let id: Option<i32> = row.get(0);  // Generic typed access
    ///         let value = row.get_f64(1);        // Direct accessor
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns [`crate::Error::Server`] if the server sends an `ErrorResponse`
    ///   while streaming the result set.
    /// - Returns [`crate::Error::Io`] on transport-level I/O failures.
    /// - Returns [`crate::Error::Conversion`] if an Arrow IPC chunk cannot be decoded.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<Row>>> {
        // Pull the next raw chunk from the underlying transport first;
        // on TCP, this is what makes the `RowDescription` bytes arrive
        // so we can cache the schema in the step below. We collect a
        // `TransportChunk` instead of a `Vec<Row>` directly so the
        // schema can be attached after we've populated the cache.
        enum TransportChunk {
            Tcp(Vec<StreamRow>),
            Arrow(Arc<RecordBatch>),
        }

        let chunk_opt: Option<TransportChunk> = match &mut self.inner {
            RowsetInner::Tcp(stream) => stream.next_chunk()?.map(TransportChunk::Tcp),
            RowsetInner::Arrow(arrow) => arrow
                .next_chunk()?
                .map(|chunk| TransportChunk::Arrow(Arc::new(chunk.into_batch()))),
            RowsetInner::Prepared(stream) => stream.next_chunk()?.map(TransportChunk::Tcp),
        };

        let Some(chunk) = chunk_opt else {
            return Ok(None);
        };

        // Populate the schema cache if not already set, then clone the
        // Arc into each Row so `Row::get::<Numeric>` and friends can
        // look up per-column precision / scale without any caller
        // having to thread the schema through manually.
        let schema = self.cached_schema_arc();
        let rows = match chunk {
            TransportChunk::Tcp(stream_rows) => stream_rows
                .into_iter()
                .map(|row| Row::from_tcp(row, schema.clone()))
                .collect(),
            TransportChunk::Arrow(batch) => (0..batch.num_rows())
                .map(|row_index| Row::from_arrow(Arc::clone(&batch), row_index, schema.clone()))
                .collect(),
        };
        Ok(Some(rows))
    }

    /// Returns an iterator over all rows in the result set.
    ///
    /// This provides a C++-like iteration experience while maintaining
    /// Rust's explicit error handling. Chunks are fetched internally
    /// as needed, keeping memory usage constant.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// // Simple iteration (like C++)
    /// let result = conn.execute_query("SELECT * FROM users")?;
    /// for row in result.rows() {
    ///     let row = row?;  // Handle potential network errors
    ///     let id: Option<i32> = row.get(0);
    ///     let name: Option<String> = row.get(1);
    ///     println!("User: {:?} - {:?}", id, name);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Error Handling
    ///
    /// Unlike C++ which uses exceptions, Rust requires explicit error handling.
    /// Each item in the iterator is a `Result<LightweightRow>` to handle
    /// potential network or protocol errors during streaming.
    ///
    /// # Comparison with `next_chunk()`
    ///
    /// | Aspect | `rows()` | `next_chunk()` |
    /// |--------|----------|----------------|
    /// | Syntax | Simpler, C++-like | More verbose |
    /// | Error handling | Per-row with `?` | Per-chunk |
    /// | Batch ops | Use `.collect()` | Natural |
    /// | Best for | Simple iteration | Batch processing |
    #[must_use]
    pub fn rows(self) -> RowIterator<'conn> {
        RowIterator {
            rowset: self,
            current_iter: Vec::new().into_iter(),
        }
    }

    /// Collects all rows into a Vec.
    ///
    /// This is a convenience method that handles error collection more elegantly
    /// than the standard `collect::<Result<Vec<_>, _>>()` pattern.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let result = conn.execute_query("SELECT id, name FROM users")?;
    /// let rows = result.collect_rows()?;  // Much cleaner than collect::<Result<Vec<_>, _>>()
    ///
    /// for row in rows {
    ///     let id: Option<i32> = row.get(0);
    ///     let name: Option<String> = row.get(1);
    ///     println!("User: {:?} - {:?}", id, name);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the first error produced by [`next_chunk`](Self::next_chunk)
    /// while draining the stream (transport I/O failure or server-side
    /// error).
    pub fn collect_rows(self) -> crate::error::Result<Vec<Row>> {
        self.rows().collect::<crate::error::Result<Vec<_>>>()
    }

    /// Collects the first column of each row into a Vec.
    ///
    /// This is useful for single-column queries or when you only need one column.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let result = conn.execute_query("SELECT name FROM users")?;
    /// let names: Vec<Option<String>> = result.collect_column()?;
    ///
    /// for name in names {
    ///     if let Some(name) = name {
    ///         println!("User: {}", name);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the first streaming error from
    /// [`next_chunk`](Self::next_chunk). SQL `NULL` cells yield
    /// `Option::None` entries, not errors.
    pub fn collect_column<T: crate::result::RowValue>(
        self,
    ) -> crate::error::Result<Vec<Option<T>>> {
        self.rows()
            .map(|row| row.map(|r| r.get::<T>(0)))
            .collect::<crate::error::Result<Vec<_>>>()
    }

    /// Collects the first column, filtering out NULL values.
    ///
    /// This is useful when you know the column doesn't contain NULLs or want to ignore them.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let result = conn.execute_query("SELECT name FROM users WHERE name IS NOT NULL")?;
    /// let names: Vec<String> = result.collect_column_non_null()?;
    ///
    /// for name in names {
    ///     println!("User: {}", name);  // No need to handle Option
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the first streaming error from
    /// [`collect_column`](Self::collect_column).
    pub fn collect_column_non_null<T: crate::result::RowValue>(
        self,
    ) -> crate::error::Result<Vec<T>> {
        Ok(self.collect_column::<T>()?.into_iter().flatten().collect())
    }

    /// Gets the first row of the result set.
    ///
    /// This is useful for queries that are expected to return exactly one row,
    /// such as aggregate queries or lookups by unique key.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let result = conn.execute_query("SELECT COUNT(*) FROM users")?;
    /// if let Some(row) = result.first_row()? {
    ///     let count: Option<i64> = row.get(0);
    ///     println!("User count: {:?}", count);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the error from [`next_chunk`](Self::next_chunk). An empty
    /// result set yields `Ok(None)`, not an error.
    pub fn first_row(mut self) -> crate::error::Result<Option<Row>> {
        if let Some(chunk) = self.next_chunk()? {
            Ok(chunk.into_iter().next())
        } else {
            Ok(None)
        }
    }

    /// Gets the first row or returns an error if no rows were found.
    ///
    /// This is useful when you expect exactly one row and want to fail if that's not the case.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let result = conn.execute_query("SELECT id, name FROM users WHERE id = 1")?;
    /// let row = result.require_first_row()?;  // Fails if no row found
    /// let id: Option<i32> = row.get(0);
    /// let name: Option<String> = row.get(1);
    /// println!("Found user: {:?} - {:?}", id, name);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns the error from [`first_row`](Self::first_row).
    /// - Returns [`crate::Error::Conversion`] with message `"Query returned no rows"`
    ///   if the result set is empty.
    pub fn require_first_row(self) -> crate::error::Result<Row> {
        self.first_row()?
            .ok_or_else(|| crate::error::Error::conversion("Query returned no rows"))
    }

    /// Gets a scalar value from the first row, first column.
    ///
    /// This is a convenience method for scalar queries like `SELECT COUNT(*)` or `SELECT MAX(id)`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let result = conn.execute_query("SELECT COUNT(*) FROM users")?;
    /// let count: Option<i64> = result.scalar()?;  // Much cleaner than manual row handling
    /// println!("User count: {:?}", count);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the error from [`require_first_row`](Self::require_first_row):
    /// streaming error or empty result. SQL `NULL` in the single cell
    /// yields `Ok(None)`.
    pub fn scalar<T: crate::result::RowValue>(self) -> crate::error::Result<Option<T>> {
        Ok(self.require_first_row()?.get(0))
    }

    /// Gets a scalar value from the first row, first column, or returns an error if NULL.
    ///
    /// This is useful when you expect a non-NULL scalar result.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let result = conn.execute_query("SELECT COUNT(*) FROM users")?;
    /// let count: i64 = result.require_scalar()?;  // Fails if NULL
    /// println!("User count: {}", count);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns the error from [`scalar`](Self::scalar).
    /// - Returns [`crate::Error::Conversion`] with message `"Scalar query returned NULL"`
    ///   if the single cell is SQL `NULL`.
    pub fn require_scalar<T: crate::result::RowValue>(self) -> crate::error::Result<T> {
        self.scalar()?
            .ok_or_else(|| crate::error::Error::conversion("Scalar query returned NULL"))
    }
}

// =============================================================================
// RowIterator - C++-like iteration over query results
// =============================================================================

/// An iterator over rows in a query result set.
///
/// `RowIterator` provides a C++-like iteration experience, hiding the
/// chunked fetching internally. Each call to `next()` returns the next
/// row, automatically fetching new chunks as needed.
///
/// # Memory Behavior
///
/// Memory usage remains constant regardless of result set size:
/// - Internally fetches 64K rows at a time
/// - Previous chunks are dropped when exhausted
/// - Safe for billion-row results
///
/// # Example
///
/// ```no_run
/// # use hyperdb_api::{Connection, Result};
/// # fn example(conn: &Connection) -> Result<()> {
/// let result = conn.execute_query("SELECT id, name FROM users")?;
/// for row in result.rows() {
///     let row = row?;
///     let id = row.get_i32(0).unwrap_or(-1);
///     let name = row.get::<String>(1).unwrap_or_default();
///     println!("{}: {}", id, name);
/// }
/// # Ok(())
/// # }
/// ```
///
/// # Error Handling
///
/// Each iteration yields a `Result<Row>`. Errors can occur
/// when fetching new chunks from the server (network issues, protocol
/// errors, etc.). Use `?` or match to handle them:
///
/// ```no_run
/// # use hyperdb_api::{Rowset, Result};
/// # fn example(mut result: Rowset) -> Result<()> {
/// // Using ? in a function that returns Result
/// for row in result.rows() {
///     let row = row?;
///     // process row...
/// }
/// # Ok(())
/// # }
/// # fn example2(mut result: Rowset) -> Result<()> {
/// // Using try_for_each
/// result.rows().try_for_each(|row| -> Result<()> {
///     let row = row?;
///     // process row...
///     Ok(())
/// })?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct RowIterator<'conn> {
    rowset: Rowset<'conn>,
    current_iter: std::vec::IntoIter<Row>,
}

impl Iterator for RowIterator<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        // Try to get next row from current chunk
        if let Some(row) = self.current_iter.next() {
            return Some(Ok(row));
        }

        // Current chunk exhausted, fetch next chunk
        match self.rowset.next_chunk() {
            Ok(Some(chunk)) => {
                self.current_iter = chunk.into_iter();
                // Return first row of new chunk
                self.current_iter.next().map(Ok)
            }
            Ok(None) => None,       // No more rows
            Err(e) => Some(Err(e)), // Error fetching chunk
        }
    }
}

// =============================================================================
// Unit tests that don't need a live hyperd backend.
//
// Anything requiring a real Hyper process lives in `hyperdb-api/tests/*.rs` where
// `TestConnection` spins up a `HyperProcess` per test. These tests exercise
// pure in-process logic — specifically the Arrow-path branches of
// `Row::get_numeric`, where we can construct a synthetic `RecordBatch` with a
// specific `DataType::Decimal128(p, s)` descriptor and probe `Row`'s
// handling of it without hyperd in the loop.
// =============================================================================

#[cfg(test)]
mod arrow_path_tests {
    use super::*;
    use arrow::array::Decimal128Array;
    use arrow::datatypes::{DataType as ArrowType, Field, Schema};

    /// Build a single-row `RecordBatch` with a Decimal128 column whose
    /// value is `raw` and whose precision/scale are those passed in.
    fn decimal128_batch(raw: i128, precision: u8, scale: i8) -> Arc<RecordBatch> {
        let array = Decimal128Array::from(vec![Some(raw)])
            .with_precision_and_scale(precision, scale)
            .expect("valid Arrow Decimal128");
        let field = Field::new("v", ArrowType::Decimal128(precision, scale), true);
        let schema = Arc::new(Schema::new(vec![field]));
        Arc::new(RecordBatch::try_new(schema, vec![Arc::new(array)]).expect("batch"))
    }

    /// Happy-path: a positive-scale Arrow Decimal128 decodes correctly
    /// via `row.get::<Numeric>()`, locking in the common case alongside
    /// the negative-scale test below.
    #[test]
    fn get_numeric_reads_arrow_decimal128_with_positive_scale() {
        // NUMERIC(10, 2), unscaled value 123 → 1.23
        let batch = decimal128_batch(123, 10, 2);
        let row = Row::from_arrow(Arc::clone(&batch), 0, None);

        let numeric = row.get_numeric(0).expect("Some for positive-scale decimal");
        assert_eq!(numeric.unscaled_value(), 123);
        assert_eq!(numeric.scale(), 2);
        assert!((numeric.to_f64() - 1.23).abs() < 1e-9);

        // Same result via the generic `row.get::<Numeric>` path.
        let via_rowvalue: hyperdb_api_core::types::Numeric =
            row.get(0).expect("RowValue path agrees with get_numeric");
        assert_eq!(via_rowvalue, numeric);
    }

    /// Arrow's `DataType::Decimal128(u8, i8)` allows negative scale —
    /// a legitimate Arrow concept meaning "raw × 10^abs(scale)" (e.g.
    /// scale=-2 with raw=5 renders as 500). Hyper's `Numeric` uses a
    /// `u8` scale with no representation for that multiplier.
    ///
    /// The earlier `.max(0) as u8` code silently clamped the scale to
    /// 0 while keeping `raw` unchanged — which produces a value with
    /// the wrong magnitude (`5` instead of `500` in the example
    /// above). The fix here is to reject negative scales via
    /// `try_into` + `?`, which surfaces as `None` to the caller.
    /// That's strictly safer than a silent-wrong-magnitude value.
    #[test]
    fn get_numeric_rejects_arrow_decimal128_with_negative_scale() {
        // NUMERIC(10, -2) — Arrow allows this; Hyper's Numeric can't
        // represent it. Our `get_numeric` must return None rather
        // than silently drop the negative-scale multiplier.
        let batch = decimal128_batch(5, 10, -2);
        let row = Row::from_arrow(Arc::clone(&batch), 0, None);

        assert!(
            row.get_numeric(0).is_none(),
            "negative Arrow scale must not produce a silently-wrong-magnitude Numeric",
        );

        // And the same through the `RowValue` blanket path.
        let via_rowvalue: Option<hyperdb_api_core::types::Numeric> = row.get(0);
        assert!(via_rowvalue.is_none());
    }

    /// Boundary: scale = 0 is a legal `u8` and must still succeed.
    /// Guards against an over-tightened check that accidentally
    /// rejects zero along with negatives.
    #[test]
    fn get_numeric_accepts_arrow_decimal128_with_zero_scale() {
        let batch = decimal128_batch(42, 10, 0);
        let row = Row::from_arrow(Arc::clone(&batch), 0, None);
        let numeric = row.get_numeric(0).expect("scale 0 is fine");
        assert_eq!(numeric.unscaled_value(), 42);
        assert_eq!(numeric.scale(), 0);
    }
}
