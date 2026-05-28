// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Arrow IPC result parsing for unified query results.
//!
//! This module provides utilities for parsing Arrow IPC data into
//! row-based iteration, enabling a consistent API across TCP and gRPC transports.
//!
//! # Zero-copy decoding
//!
//! The parsing entry points (`ArrowRowset::from_bytes`, `from_buffer`,
//! `from_chunks`, and `parse_arrow_ipc`) feed Arrow's `StreamDecoder` directly
//! from a shared buffer. Record-batch columnar buffers share the input
//! allocation, so fixed-width primitive columns are genuinely zero-copy from
//! the HTTP/2 frame (for gRPC) or from the COPY response buffer (for TCP)
//! all the way to the `RecordBatch`.

#![expect(
    dead_code,
    reason = "experimental zero-copy Arrow path; helpers retained for upcoming gRPC/TCP wiring"
)]

use std::collections::VecDeque;
use std::sync::Arc;

use arrow::array::{
    Array, BinaryArray, BooleanArray, Date32Array, Float32Array, Float64Array, Int16Array,
    Int32Array, Int64Array, LargeBinaryArray, LargeStringArray, StringArray,
    TimestampMicrosecondArray,
};
use arrow::buffer::Buffer;
use arrow::datatypes::{DataType, Schema, TimeUnit};
use arrow::ipc::reader::StreamDecoder;
use arrow::record_batch::RecordBatch;
use bytes::Bytes;

use crate::error::{Error, Result};
use hyperdb_api_core::types::SqlType;

/// A row from an Arrow record batch, providing typed value access.
#[derive(Debug)]
pub struct ArrowRow<'a> {
    batch: &'a RecordBatch,
    row_index: usize,
}

impl<'a> ArrowRow<'a> {
    /// Creates a new `ArrowRow` referencing a specific row in a batch.
    pub(crate) fn new(batch: &'a RecordBatch, row_index: usize) -> Self {
        ArrowRow { batch, row_index }
    }

    /// Returns the number of columns in this row.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.batch.num_columns()
    }

    /// Gets a value at the given column index, with type conversion.
    #[must_use]
    pub fn get<T: FromArrowValue>(&self, col: usize) -> Option<T> {
        if col >= self.batch.num_columns() {
            return None;
        }
        T::from_arrow_column(self.batch.column(col), self.row_index)
    }

    /// Gets an i16 value at the given column index.
    #[must_use]
    pub fn get_i16(&self, col: usize) -> Option<i16> {
        self.get::<i16>(col)
    }

    /// Gets an i32 value at the given column index.
    #[must_use]
    pub fn get_i32(&self, col: usize) -> Option<i32> {
        self.get::<i32>(col)
    }

    /// Gets an i64 value at the given column index.
    #[must_use]
    pub fn get_i64(&self, col: usize) -> Option<i64> {
        self.get::<i64>(col)
    }

    /// Gets an f32 value at the given column index.
    #[must_use]
    pub fn get_f32(&self, col: usize) -> Option<f32> {
        self.get::<f32>(col)
    }

    /// Gets an f64 value at the given column index.
    #[must_use]
    pub fn get_f64(&self, col: usize) -> Option<f64> {
        self.get::<f64>(col)
    }

    /// Gets a bool value at the given column index.
    #[must_use]
    pub fn get_bool(&self, col: usize) -> Option<bool> {
        self.get::<bool>(col)
    }

    /// Gets a String value at the given column index.
    #[must_use]
    pub fn get_string(&self, col: usize) -> Option<String> {
        self.get::<String>(col)
    }

    /// Gets bytes at the given column index.
    #[must_use]
    pub fn get_bytes(&self, col: usize) -> Option<Vec<u8>> {
        self.get::<Vec<u8>>(col)
    }

    /// Checks if the value at the given column is null.
    #[must_use]
    pub fn is_null(&self, col: usize) -> bool {
        if col >= self.batch.num_columns() {
            return true;
        }
        self.batch.column(col).is_null(self.row_index)
    }
}

/// Trait for types that can be extracted from Arrow columns.
pub trait FromArrowValue: Sized {
    /// Extract a value from an Arrow array at the given row index.
    fn from_arrow_column(array: &Arc<dyn Array>, row: usize) -> Option<Self>;
}

impl FromArrowValue for i16 {
    fn from_arrow_column(array: &Arc<dyn Array>, row: usize) -> Option<Self> {
        if array.is_null(row) {
            return None;
        }
        if let Some(arr) = array.as_any().downcast_ref::<Int16Array>() {
            Some(arr.value(row))
        } else if let Some(arr) = array.as_any().downcast_ref::<Int32Array>() {
            i16::try_from(arr.value(row)).ok()
        } else {
            array
                .as_any()
                .downcast_ref::<Int64Array>()
                .and_then(|arr| i16::try_from(arr.value(row)).ok())
        }
    }
}

impl FromArrowValue for i32 {
    fn from_arrow_column(array: &Arc<dyn Array>, row: usize) -> Option<Self> {
        if array.is_null(row) {
            return None;
        }
        if let Some(arr) = array.as_any().downcast_ref::<Int32Array>() {
            Some(arr.value(row))
        } else if let Some(arr) = array.as_any().downcast_ref::<Int16Array>() {
            Some(i32::from(arr.value(row)))
        } else {
            array
                .as_any()
                .downcast_ref::<Int64Array>()
                .and_then(|arr| i32::try_from(arr.value(row)).ok())
        }
    }
}

impl FromArrowValue for i64 {
    fn from_arrow_column(array: &Arc<dyn Array>, row: usize) -> Option<Self> {
        if array.is_null(row) {
            return None;
        }
        if let Some(arr) = array.as_any().downcast_ref::<Int64Array>() {
            Some(arr.value(row))
        } else if let Some(arr) = array.as_any().downcast_ref::<Int32Array>() {
            Some(i64::from(arr.value(row)))
        } else if let Some(arr) = array.as_any().downcast_ref::<Int16Array>() {
            Some(i64::from(arr.value(row)))
        } else if let Some(arr) = array.as_any().downcast_ref::<Date32Array>() {
            Some(i64::from(arr.value(row)))
        } else {
            array
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .map(|arr| arr.value(row))
        }
    }
}

impl FromArrowValue for f32 {
    fn from_arrow_column(array: &Arc<dyn Array>, row: usize) -> Option<Self> {
        if array.is_null(row) {
            return None;
        }
        if let Some(arr) = array.as_any().downcast_ref::<Float32Array>() {
            Some(arr.value(row))
        } else {
            array.as_any().downcast_ref::<Float64Array>().map(|arr| {
                // Narrowing f64 → f32 is an inherent precision loss across the
                // Float64 column path. Callers that need full precision should
                // use the f64 accessor; this path preserves the historical
                // best-effort behavior.
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "f64 → f32 narrowing is caller-accepted precision loss for this column-coercion path"
                )]
                let narrowed = arr.value(row) as f32;
                narrowed
            })
        }
    }
}

impl FromArrowValue for f64 {
    fn from_arrow_column(array: &Arc<dyn Array>, row: usize) -> Option<Self> {
        if array.is_null(row) {
            return None;
        }
        if let Some(arr) = array.as_any().downcast_ref::<Float64Array>() {
            Some(arr.value(row))
        } else {
            array
                .as_any()
                .downcast_ref::<Float32Array>()
                .map(|arr| f64::from(arr.value(row)))
        }
    }
}

impl FromArrowValue for bool {
    fn from_arrow_column(array: &Arc<dyn Array>, row: usize) -> Option<Self> {
        if array.is_null(row) {
            return None;
        }
        array
            .as_any()
            .downcast_ref::<BooleanArray>()
            .map(|arr| arr.value(row))
    }
}

impl FromArrowValue for String {
    fn from_arrow_column(array: &Arc<dyn Array>, row: usize) -> Option<Self> {
        if array.is_null(row) {
            return None;
        }
        if let Some(arr) = array.as_any().downcast_ref::<StringArray>() {
            Some(arr.value(row).to_string())
        } else {
            array
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .map(|arr| arr.value(row).to_string())
        }
    }
}

impl FromArrowValue for Vec<u8> {
    fn from_arrow_column(array: &Arc<dyn Array>, row: usize) -> Option<Self> {
        if array.is_null(row) {
            return None;
        }
        if let Some(arr) = array.as_any().downcast_ref::<BinaryArray>() {
            Some(arr.value(row).to_vec())
        } else {
            array
                .as_any()
                .downcast_ref::<LargeBinaryArray>()
                .map(|arr| arr.value(row).to_vec())
        }
    }
}

/// A chunk of rows from Arrow data, analogous to TCP's row chunks.
#[derive(Debug)]
pub struct ArrowChunk {
    batch: RecordBatch,
}

impl ArrowChunk {
    /// Creates a new `ArrowChunk` from a `RecordBatch`.
    pub(crate) fn new(batch: RecordBatch) -> Self {
        ArrowChunk { batch }
    }

    /// Returns the number of rows in this chunk.
    #[must_use]
    pub fn len(&self) -> usize {
        self.batch.num_rows()
    }

    /// Returns true if this chunk has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.batch.num_rows() == 0
    }

    /// Returns the number of columns.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.batch.num_columns()
    }

    /// Gets the row at the given index.
    #[must_use]
    pub fn row(&self, index: usize) -> Option<ArrowRow<'_>> {
        if index < self.batch.num_rows() {
            Some(ArrowRow::new(&self.batch, index))
        } else {
            None
        }
    }

    /// Returns the first row, if any.
    #[must_use]
    pub fn first(&self) -> Option<ArrowRow<'_>> {
        self.row(0)
    }

    /// Returns an iterator over the rows.
    #[must_use]
    pub fn iter(&self) -> ArrowChunkIter<'_> {
        ArrowChunkIter {
            chunk: self,
            index: 0,
        }
    }

    /// Consumes the chunk and returns the underlying `RecordBatch`.
    #[must_use]
    pub fn into_batch(self) -> RecordBatch {
        self.batch
    }
}

impl<'a> IntoIterator for &'a ArrowChunk {
    type Item = ArrowRow<'a>;
    type IntoIter = ArrowChunkIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over rows in an `ArrowChunk`.
#[derive(Debug)]
pub struct ArrowChunkIter<'a> {
    chunk: &'a ArrowChunk,
    index: usize,
}

impl<'a> Iterator for ArrowChunkIter<'a> {
    type Item = ArrowRow<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.chunk.len() {
            let row = ArrowRow::new(&self.chunk.batch, self.index);
            self.index += 1;
            Some(row)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.chunk.len() - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ArrowChunkIter<'_> {}

/// Source of Arrow IPC byte chunks for streaming decode.
///
/// Implement this for any blocking chunk producer. The canonical use is
/// wrapping a gRPC chunk stream so `ArrowRowset` can decode record batches
/// lazily, keeping peak memory bounded by roughly one chunk regardless of
/// total result size.
///
/// Returning `Ok(None)` signals end-of-stream.
pub trait ChunkSource: Send {
    /// Returns the next Arrow IPC byte chunk, or `Ok(None)` once the source
    /// is exhausted. Subsequent calls after `Ok(None)` should keep
    /// returning `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Implementations return whatever transport error the underlying
    /// source produces (typically [`Error::Server`] from a gRPC stream or
    /// [`Error::Io`] on network failures).
    fn next_chunk(&mut self) -> Result<Option<Bytes>>;
}

/// Parsed Arrow result set for streaming row access.
///
/// Constructed either from fully materialized bytes
/// ([`from_bytes`](Self::from_bytes), [`from_buffer`](Self::from_buffer),
/// [`from_chunks`](Self::from_chunks), [`from_ipc_slice`](Self::from_ipc_slice))
/// or from a lazy chunk source
/// ([`from_stream`](Self::from_stream)).
///
/// The lazy constructor pulls and decodes chunks on demand from
/// [`next_chunk`](Self::next_chunk), so for very large result sets (GB-class
/// gRPC query results) peak client memory is bounded by roughly one chunk
/// plus whatever batches the caller is holding, rather than growing to the
/// full result size.
pub struct ArrowRowset {
    inner: ArrowRowsetInner,
    schema: Arc<Schema>,
}

impl std::fmt::Debug for ArrowRowset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArrowRowset")
            .field("schema", &self.schema)
            .finish_non_exhaustive()
    }
}

enum ArrowRowsetInner {
    /// All batches are already decoded and held in `batches`; `current` is
    /// the next index to return from `next_chunk`.
    Buffered {
        batches: Vec<RecordBatch>,
        current: usize,
    },
    /// Lazy: pull bytes from `source` and decode record batches on demand.
    ///
    /// Keeps a single `StreamDecoder` across source chunks so its schema
    /// state carries over — hyperd typically sends the schema only in the
    /// first chunk and follows up with batch-only continuation chunks.
    /// When the decoder hits an EOS marker or a second schema message, we
    /// swap in a fresh decoder (but keep the already-learned schema) so we
    /// also tolerate the "multiple concatenated IPC streams" shape.
    ///
    /// `leftover` carries unconsumed bytes from the previous chunk so
    /// messages split across chunk boundaries are reassembled.
    Streaming {
        source: Box<dyn ChunkSource>,
        decoder: StreamDecoder,
        pending: VecDeque<RecordBatch>,
        leftover: Option<Buffer>,
        exhausted: bool,
    },
}

impl ArrowRowset {
    /// Empty rowset (no schema, no batches).
    fn empty() -> Self {
        ArrowRowset {
            inner: ArrowRowsetInner::Buffered {
                batches: Vec::new(),
                current: 0,
            },
            schema: Arc::new(Schema::empty()),
        }
    }

    /// Parse Arrow IPC bytes from a shared `Bytes` handle (zero-copy).
    ///
    /// Tolerant of two shapes:
    /// - a single continuous Arrow IPC stream (what libpq COPY TO STDOUT
    ///   with `arrowstream` format produces), or
    /// - one or more self-contained streams concatenated end-to-end (what
    ///   hyperd's gRPC `execute_query_to_arrow` produces when the server
    ///   split the result across multiple `BinaryPart` messages —
    ///   `into_arrow_data` glued them together).
    ///
    /// Arrow record batches reference the same allocation as the input
    /// `Bytes`, so fixed-width columns do not incur any memcpy. Prefer this
    /// over [`from_ipc_slice`](Self::from_ipc_slice) whenever you already
    /// have a `Bytes` (which is the native return type of the gRPC path).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conversion`] wrapping an Arrow IPC decode error if
    /// `bytes` is not a valid Arrow IPC stream (or concatenation thereof).
    pub fn from_bytes(bytes: Bytes) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::empty());
        }
        Self::from_buffer(Buffer::from(bytes))
    }

    /// Parse Arrow IPC bytes from an arrow `Buffer` (zero-copy).
    ///
    /// See [`from_bytes`](Self::from_bytes) for how this tolerates both
    /// continuous and concatenated-stream inputs.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conversion`] wrapping an Arrow IPC decode error if
    /// `buf` is not a valid Arrow IPC stream.
    pub fn from_buffer(buf: Buffer) -> Result<Self> {
        if buf.is_empty() {
            return Ok(Self::empty());
        }
        let (schema, batches) = decode_possibly_concatenated_streams(buf)?;
        Ok(ArrowRowset {
            inner: ArrowRowsetInner::Buffered {
                batches,
                current: 0,
            },
            schema,
        })
    }

    /// Parse Arrow IPC bytes from multiple independent chunks (zero-copy).
    ///
    /// Each chunk is treated as its own self-contained Arrow IPC stream
    /// (schema + batches + optional EOS). This matches hyperd's gRPC
    /// output, where every `BinaryPart` message carries a fresh schema.
    /// For a single continuous stream, use [`from_bytes`](Self::from_bytes).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conversion`] wrapping an Arrow IPC decode error if any
    /// chunk cannot be parsed as a self-contained IPC stream.
    pub fn from_chunks<I>(chunks: I) -> Result<Self>
    where
        I: IntoIterator<Item = Bytes>,
    {
        let mut batches = Vec::new();
        let mut schema = Arc::new(Schema::empty());
        for chunk in chunks {
            if chunk.is_empty() {
                continue;
            }
            let (chunk_schema, chunk_batches) = decode_chunk(chunk)?;
            if schema.fields().is_empty() {
                schema = chunk_schema;
            }
            batches.extend(chunk_batches);
        }
        Ok(ArrowRowset {
            inner: ArrowRowsetInner::Buffered {
                batches,
                current: 0,
            },
            schema,
        })
    }

    /// Build a streaming rowset that pulls chunks from `source` on demand.
    ///
    /// Unlike the `from_*` constructors, this does **not** pre-decode the
    /// whole IPC stream up front. Each call to
    /// [`next_chunk`](Self::next_chunk) pulls just enough bytes from
    /// `source` to produce one Arrow `RecordBatch`. Peak memory is bounded
    /// by one source chunk (typically the tonic `max_decoding_message_size`
    /// default of 64 MB) plus any batches the caller is still holding —
    /// regardless of total result size.
    ///
    /// The first source chunk is pulled eagerly so that [`schema`](Self::schema)
    /// returns the real schema before the first `next_chunk` call. If the
    /// stream is empty, an empty rowset with `Schema::empty()` is returned.
    ///
    /// # Errors
    ///
    /// - Returns the transport error from `source.next_chunk()` when
    ///   priming the decoder with the first chunk.
    /// - Returns [`Error::Conversion`] wrapping an Arrow IPC decode error if
    ///   that first chunk is not a valid Arrow IPC stream prefix.
    pub fn from_stream(source: Box<dyn ChunkSource>) -> Result<Self> {
        let mut rowset = ArrowRowset {
            inner: ArrowRowsetInner::Streaming {
                source,
                decoder: StreamDecoder::new(),
                pending: VecDeque::new(),
                leftover: None,
                exhausted: false,
            },
            schema: Arc::new(Schema::empty()),
        };
        // Eagerly consume source chunks until the schema is available so
        // `schema()` returns the real schema before `next_chunk()` is
        // called. Any decoded batches go straight into `pending`.
        rowset.prime_stream()?;
        Ok(rowset)
    }

    /// Drive the streaming decoder until we have the schema (or the
    /// source is exhausted). Decoded batches land in `pending`, any
    /// leftover bytes get stashed on the rowset for the first
    /// `next_chunk` call to consume.
    fn prime_stream(&mut self) -> Result<()> {
        let new_schema = {
            let ArrowRowsetInner::Streaming {
                source,
                decoder,
                pending,
                leftover,
                exhausted,
            } = &mut self.inner
            else {
                return Ok(());
            };
            while decoder.schema().is_none() && !*exhausted {
                let mut buf = match leftover.take() {
                    Some(b) => b,
                    None => match source.next_chunk()? {
                        Some(bytes) if !bytes.is_empty() => Buffer::from(bytes),
                        Some(_) => continue,
                        None => {
                            *exhausted = true;
                            break;
                        }
                    },
                };
                drive_streaming_decoder(decoder, &mut buf, pending)?;
                if !buf.is_empty() {
                    *leftover = Some(buf);
                }
            }
            decoder.schema()
        };
        if let Some(s) = new_schema {
            self.schema = s;
        }
        Ok(())
    }

    /// Parse Arrow IPC bytes from a borrowed slice.
    ///
    /// This copies `data` into an arrow `Buffer` before decoding. Prefer
    /// [`from_bytes`](Self::from_bytes) when you already own a `Bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conversion`] wrapping an Arrow IPC decode error if
    /// `data` is not a valid Arrow IPC stream.
    pub fn from_ipc_slice(data: &[u8]) -> Result<Self> {
        if data.is_empty() {
            return Ok(Self::empty());
        }
        // `Buffer::from` over a `Vec<u8>` takes ownership without copying,
        // but we start from a borrowed slice, so we must copy once here.
        Self::from_buffer(Buffer::from(data.to_vec()))
    }

    /// Returns the schema of the result set.
    #[must_use]
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// Returns the number of columns.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.schema.fields().len()
    }

    /// Returns column names.
    #[must_use]
    pub fn column_names(&self) -> Vec<String> {
        self.schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }

    /// Returns the column name at the given index.
    #[must_use]
    pub fn column_name(&self, index: usize) -> Option<&str> {
        self.schema.fields().get(index).map(|f| f.name().as_str())
    }

    /// Gets the next chunk of rows.
    ///
    /// For buffered rowsets this walks a preallocated `Vec<RecordBatch>`.
    /// For streaming rowsets it pulls and decodes source chunks on demand
    /// until at least one record batch is ready (or the source is
    /// exhausted).
    ///
    /// # Errors
    ///
    /// For streaming rowsets:
    /// - Returns the transport error from `source.next_chunk()`.
    /// - Returns [`Error::Conversion`] wrapping an Arrow IPC decode error if a
    ///   chunk contains malformed stream bytes.
    ///
    /// Buffered rowsets never error — they walk a pre-decoded vector.
    pub fn next_chunk(&mut self) -> Result<Option<ArrowChunk>> {
        match &mut self.inner {
            ArrowRowsetInner::Buffered { batches, current } => {
                if *current >= batches.len() {
                    return Ok(None);
                }
                let batch = batches[*current].clone();
                *current += 1;
                Ok(Some(ArrowChunk::new(batch)))
            }
            ArrowRowsetInner::Streaming {
                source,
                decoder,
                pending,
                leftover,
                exhausted,
            } => loop {
                if let Some(batch) = pending.pop_front() {
                    return Ok(Some(ArrowChunk::new(batch)));
                }
                if *exhausted {
                    return Ok(None);
                }
                let mut buf = match leftover.take() {
                    Some(b) => b,
                    None => match source.next_chunk()? {
                        Some(bytes) if !bytes.is_empty() => Buffer::from(bytes),
                        Some(_) => continue,
                        None => {
                            *exhausted = true;
                            continue;
                        }
                    },
                };
                drive_streaming_decoder(decoder, &mut buf, pending)?;
                if !buf.is_empty() {
                    *leftover = Some(buf);
                }
            },
        }
    }

    /// Returns the total number of rows across all batches.
    ///
    /// For streaming rowsets this reflects only batches decoded **so far** —
    /// until [`next_chunk`](Self::next_chunk) has pulled everything from the
    /// source, the total is not yet known.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        match &self.inner {
            ArrowRowsetInner::Buffered { batches, .. } => batches
                .iter()
                .map(arrow::array::RecordBatch::num_rows)
                .sum(),
            ArrowRowsetInner::Streaming { pending, .. } => pending
                .iter()
                .map(arrow::array::RecordBatch::num_rows)
                .sum(),
        }
    }

    /// Returns true if there are no rows available **right now**.
    ///
    /// For streaming rowsets this only reflects the currently-decoded
    /// batches, not the full result — a streaming rowset that has not been
    /// iterated will usually report `is_empty() == true` even if the server
    /// will send more data on `next_chunk`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match &self.inner {
            ArrowRowsetInner::Buffered { batches, .. } => {
                batches.is_empty() || batches.iter().all(|b| b.num_rows() == 0)
            }
            ArrowRowsetInner::Streaming {
                pending, exhausted, ..
            } => *exhausted && pending.is_empty(),
        }
    }
}

/// Convert Arrow `DataType` to SQL type name string.
fn arrow_type_to_sql_name(dt: &DataType) -> String {
    match dt {
        DataType::Boolean => "BOOLEAN".to_string(),
        DataType::Int8 => "SMALLINT".to_string(),
        DataType::Int16 => "SMALLINT".to_string(),
        DataType::Int32 => "INTEGER".to_string(),
        DataType::Int64 => "BIGINT".to_string(),
        DataType::UInt8 => "SMALLINT".to_string(),
        DataType::UInt16 => "INTEGER".to_string(),
        DataType::UInt32 => "BIGINT".to_string(),
        DataType::UInt64 => "BIGINT".to_string(),
        DataType::Float16 => "REAL".to_string(),
        DataType::Float32 => "REAL".to_string(),
        DataType::Float64 => "DOUBLE PRECISION".to_string(),
        DataType::Utf8 | DataType::LargeUtf8 => "TEXT".to_string(),
        DataType::Binary | DataType::LargeBinary => "BYTEA".to_string(),
        DataType::Date32 | DataType::Date64 => "DATE".to_string(),
        DataType::Time32(_) | DataType::Time64(_) => "TIME".to_string(),
        DataType::Timestamp(TimeUnit::Microsecond, None) => "TIMESTAMP".to_string(),
        DataType::Timestamp(TimeUnit::Microsecond, Some(_)) => "TIMESTAMPTZ".to_string(),
        DataType::Timestamp(_, None) => "TIMESTAMP".to_string(),
        DataType::Timestamp(_, Some(_)) => "TIMESTAMPTZ".to_string(),
        DataType::Decimal128(p, s) => format!("NUMERIC({p}, {s})"),
        DataType::Decimal256(p, s) => format!("NUMERIC({p}, {s})"),
        DataType::Interval(_) => "INTERVAL".to_string(),
        DataType::List(_) => "ARRAY".to_string(),
        DataType::Struct(_) => "RECORD".to_string(),
        _ => "UNKNOWN".to_string(),
    }
}

/// Narrows an Arrow `Decimal*` scale (`i8`) to the `u32` scale carried by
/// `SqlType::Numeric`. Negative scales are not representable in `SqlType` and
/// indicate a schema mismatch; we clamp to `0` rather than panic so that a
/// malformed Arrow schema does not take down an entire query.
fn decimal_scale_to_u32(scale: i8) -> u32 {
    u32::try_from(scale).unwrap_or(0)
}

/// Convert Arrow `DataType` to `SqlType`.
pub(crate) fn arrow_type_to_sql_type(dt: &DataType) -> SqlType {
    match dt {
        DataType::Boolean => SqlType::Bool,
        DataType::Int8 | DataType::Int16 => SqlType::SmallInt,
        DataType::Int32 => SqlType::Int,
        DataType::Int64 => SqlType::BigInt,
        DataType::UInt8 | DataType::UInt16 => SqlType::SmallInt,
        DataType::UInt32 => SqlType::Int,
        DataType::UInt64 => SqlType::BigInt,
        DataType::Float16 | DataType::Float32 => SqlType::Float,
        DataType::Float64 => SqlType::Double,
        DataType::Utf8 | DataType::LargeUtf8 => SqlType::Text,
        DataType::Binary | DataType::LargeBinary => SqlType::ByteA,
        DataType::Date32 | DataType::Date64 => SqlType::Date,
        DataType::Time32(_) | DataType::Time64(_) => SqlType::Time,
        DataType::Timestamp(_, None) => SqlType::Timestamp,
        DataType::Timestamp(_, Some(_)) => SqlType::TimestampTz,
        DataType::Decimal128(p, s) => SqlType::Numeric {
            precision: u32::from(*p),
            scale: decimal_scale_to_u32(*s),
        },
        DataType::Decimal256(p, s) => SqlType::Numeric {
            precision: u32::from(*p),
            scale: decimal_scale_to_u32(*s),
        },
        DataType::Interval(_) => SqlType::Interval,
        _ => SqlType::Text, // Fallback to text for unknown types
    }
}

/// Parses Arrow IPC stream bytes into a vector of `RecordBatches` (zero-copy).
///
/// The returned batches share their underlying allocation with the input
/// `Bytes`, so fixed-width columns do not incur any memcpy.
///
/// # Errors
///
/// Returns [`Error::Conversion`] wrapping an Arrow IPC decode error if `bytes`
/// is not a valid Arrow IPC stream (or concatenation thereof).
pub fn parse_arrow_ipc(bytes: Bytes) -> Result<Vec<RecordBatch>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let (_, batches) = decode_possibly_concatenated_streams(Buffer::from(bytes))?;
    Ok(batches)
}

/// Decode a single self-contained Arrow IPC stream (schema + batches +
/// optional EOS) and return the schema plus the record batches. Used per
/// chunk on the streaming / gRPC path. Tolerates trailing EOS bytes and
/// trailing extra streams by delegating to
/// `decode_possibly_concatenated_streams`.
fn decode_chunk(bytes: Bytes) -> Result<(Arc<Schema>, Vec<RecordBatch>)> {
    decode_possibly_concatenated_streams(Buffer::from(bytes))
}

/// Arrow IPC end-of-stream marker: continuation tag `0xFFFFFFFF` followed
/// by a zero length. When `StreamDecoder` consumes this marker it moves
/// to its `Finished` state and subsequent calls fail with
/// "Unexpected EOS"; detecting the marker ourselves lets us spin up a
/// fresh decoder to start the next stream.
const ARROW_IPC_EOS: [u8; 8] = [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0];

/// Run `decoder` over `buf`, pushing every decoded record batch onto
/// `pending`. Advances `buf` as bytes are consumed; stops when `buf` is
/// empty, the decoder signals it needs more data, or we hit a stream
/// boundary that requires a fresh decoder.
///
/// Stream boundaries we recognize:
/// - `ARROW_IPC_EOS` marker at the start of `buf` — consume the 8 bytes
///   and swap `*decoder` for a fresh one so the next stream can start.
/// - "Not expecting a schema when messages are read" error from the
///   decoder — hyperd sometimes concatenates streams without an EOS in
///   between. Roll `buf` back to before the decode call and swap the
///   decoder; the caller will re-enter this function to decode the new
///   stream.
fn drive_streaming_decoder(
    decoder: &mut StreamDecoder,
    buf: &mut Buffer,
    pending: &mut VecDeque<RecordBatch>,
) -> Result<()> {
    loop {
        if buf.is_empty() {
            return Ok(());
        }
        // EOS marker at the head → consume it. If there are more bytes
        // after it, reset the decoder to start a fresh stream. Preserve
        // the decoder (and its schema) if this is a trailing EOS with no
        // more data, since the caller may pull more bytes later that
        // continue using the same schema.
        if buf.len() >= ARROW_IPC_EOS.len() && buf[..ARROW_IPC_EOS.len()] == ARROW_IPC_EOS {
            let new_len = buf.len() - ARROW_IPC_EOS.len();
            *buf = buf.slice_with_length(ARROW_IPC_EOS.len(), new_len);
            if !buf.is_empty() {
                *decoder = StreamDecoder::new();
            }
            continue;
        }
        // When we already have a schema and the next message is another
        // schema (hyperd emits repeated schemas at chunk boundaries),
        // peek the message size and skip past it rather than resetting
        // the decoder — resetting would discard the schema and make the
        // decoder reject the next RecordBatch with "Missing schema".
        if decoder.schema().is_some() && peek_is_schema_message(buf) {
            match peek_message_total_size(buf) {
                Some(total) if buf.len() >= total => {
                    *buf = buf.slice_with_length(total, buf.len() - total);
                    continue;
                }
                _ => {}
            }
        }
        let buf_before = buf.clone();
        match decoder.decode(buf) {
            Ok(Some(batch)) => pending.push_back(batch),
            Ok(None) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("Not expecting a schema when messages are read") {
                    // Roll back and swap the decoder; the next iteration
                    // will start a fresh stream.
                    *buf = buf_before;
                    *decoder = StreamDecoder::new();
                    continue;
                }
                if msg.contains("Unexpected EOS") {
                    // Decoder already in Finished state; roll back and
                    // swap. Keep any pending bytes (probably a new
                    // stream start) for the next pass.
                    *buf = buf_before;
                    *decoder = StreamDecoder::new();
                    continue;
                }
                return Err(Error::conversion(format!(
                    "Failed to parse Arrow IPC data: {e}"
                )));
            }
        }
    }
}

/// Peeks at the first message of `buf` and returns `true` if it is an
/// Arrow IPC Schema message (so we can skip repeated schemas emitted at
/// chunk boundaries). Only inspects the flatbuffer header type byte.
fn peek_is_schema_message(buf: &Buffer) -> bool {
    // Header layout: 4-byte continuation marker + 4-byte message length +
    // `length` bytes of flatbuffer message. Inside the flatbuffer, the
    // message `header_type` is an enum where Schema = 1 (see the Arrow
    // IPC spec).
    let Some((_len, body)) = peek_message_body(buf) else {
        return false;
    };
    // Root table offset is the first 4 bytes of the message. The type
    // byte lives at offset root + vtable[header_type]. Parsing the full
    // flatbuffer just to check one enum is overkill, so we use
    // `arrow_ipc::root_as_message` lightly — if parsing fails, fall back
    // to letting the decoder handle it.
    match arrow::ipc::root_as_message(body) {
        Ok(msg) => msg.header_type() == arrow::ipc::MessageHeader::Schema,
        Err(_) => false,
    }
}

/// Returns `Some((message_len, message_body_bytes))` if `buf` starts with
/// a full Arrow IPC framed message whose flatbuffer body is present.
fn peek_message_body(buf: &Buffer) -> Option<(usize, &[u8])> {
    let bytes: &[u8] = buf;
    // Optional continuation marker.
    let (length_offset, remaining) = if bytes.len() >= 4 && bytes[0..4] == [0xFF; 4] {
        (4, &bytes[4..])
    } else {
        (0, bytes)
    };
    if remaining.len() < 4 {
        return None;
    }
    let length =
        u32::from_le_bytes([remaining[0], remaining[1], remaining[2], remaining[3]]) as usize;
    let body_start = length_offset + 4;
    if buf.len() < body_start + length {
        return None;
    }
    Some((body_start + length, &bytes[body_start..body_start + length]))
}

/// Returns the total byte size of the first framed Arrow IPC message in
/// `buf` — the continuation marker (if present) + 4-byte length +
/// flatbuffer body + any body bytes signalled by the flatbuffer. We only
/// use this for schema messages, which have zero body bytes.
fn peek_message_total_size(buf: &Buffer) -> Option<usize> {
    let (total, _body) = peek_message_body(buf)?;
    Some(total)
}

/// Decode an arrow Buffer that may be one continuous Arrow IPC stream,
/// multiple self-contained streams concatenated end to end, or multiple
/// continuation streams that share a schema from the first one. Handles
/// all three shapes via `drive_streaming_decoder`, which carries decoder
/// state across stream boundaries.
fn decode_possibly_concatenated_streams(
    mut buf: Buffer,
) -> Result<(Arc<Schema>, Vec<RecordBatch>)> {
    let mut decoder = StreamDecoder::new();
    let mut pending = VecDeque::new();
    while !buf.is_empty() {
        let before_len = buf.len();
        drive_streaming_decoder(&mut decoder, &mut buf, &mut pending)?;
        if buf.len() == before_len {
            // drive_streaming_decoder consumed nothing; we're either
            // done (if buf is empty) or stuck (malformed input).
            if !buf.is_empty() {
                return Err(Error::conversion(
                    "Failed to parse Arrow IPC data: decoder made no progress",
                ));
            }
            break;
        }
    }
    let schema = decoder
        .schema()
        .unwrap_or_else(|| Arc::new(Schema::empty()));
    Ok((schema, pending.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::Field;

    #[test]
    fn test_arrow_rowset_empty() {
        let rowset = ArrowRowset::from_bytes(Bytes::new()).unwrap();
        assert!(rowset.is_empty());
        assert_eq!(rowset.column_count(), 0);

        let rowset = ArrowRowset::from_ipc_slice(&[]).unwrap();
        assert!(rowset.is_empty());
    }

    #[test]
    fn test_arrow_chunk_iteration() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
        ]));

        let id_array = Int32Array::from(vec![1, 2, 3]);
        let name_array = StringArray::from(vec![Some("Alice"), Some("Bob"), None]);

        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(id_array), Arc::new(name_array)]).unwrap();

        let chunk = ArrowChunk::new(batch);
        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk.column_count(), 2);

        let mut iter = chunk.iter();

        let row0 = iter.next().unwrap();
        assert_eq!(row0.get::<i32>(0), Some(1));
        assert_eq!(row0.get::<String>(1), Some("Alice".to_string()));

        let row1 = iter.next().unwrap();
        assert_eq!(row1.get::<i32>(0), Some(2));
        assert_eq!(row1.get::<String>(1), Some("Bob".to_string()));

        let row2 = iter.next().unwrap();
        assert_eq!(row2.get::<i32>(0), Some(3));
        assert_eq!(row2.get::<String>(1), None);
        assert!(row2.is_null(1));

        assert!(iter.next().is_none());
    }

    /// A `ChunkSource` backed by a pre-populated `VecDeque<Bytes>`, used to
    /// exercise the streaming path in tests.
    struct VecChunkSource {
        chunks: VecDeque<Bytes>,
    }

    impl VecChunkSource {
        fn new(chunks: Vec<Bytes>) -> Self {
            VecChunkSource {
                chunks: chunks.into(),
            }
        }
    }

    impl ChunkSource for VecChunkSource {
        fn next_chunk(&mut self) -> Result<Option<Bytes>> {
            Ok(self.chunks.pop_front())
        }
    }

    /// Builds `num_streams` self-contained Arrow IPC streams, each with
    /// the same schema and `rows_per_stream` rows, and returns them as a
    /// `Vec<Bytes>`. Mirrors the shape of real gRPC `BinaryPart` chunks:
    /// every chunk is its own complete IPC stream.
    fn serialize_independent_streams(num_streams: usize, rows_per_stream: i32) -> Vec<Bytes> {
        use arrow::ipc::writer::StreamWriter;
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let mut out = Vec::with_capacity(num_streams);
        for s in 0..num_streams {
            let start = i32::try_from(s).expect("test uses small stream counts") * rows_per_stream;
            let id_array = Int32Array::from((start..start + rows_per_stream).collect::<Vec<_>>());
            let name_array = StringArray::from(
                (start..start + rows_per_stream)
                    .map(|i| Some(format!("n{i}")))
                    .collect::<Vec<_>>(),
            );
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![Arc::new(id_array), Arc::new(name_array)],
            )
            .unwrap();

            let mut buf: Vec<u8> = Vec::new();
            {
                let mut writer = StreamWriter::try_new(&mut buf, &schema).unwrap();
                writer.write(&batch).unwrap();
                writer.finish().unwrap();
            }
            out.push(Bytes::from(buf));
        }
        out
    }

    #[test]
    fn test_streaming_rowset_single_chunk() {
        let chunks = serialize_independent_streams(1, 100);
        let source = Box::new(VecChunkSource::new(chunks));
        let mut rowset = ArrowRowset::from_stream(source).unwrap();

        // Schema is primed eagerly so it's available before next_chunk.
        assert_eq!(rowset.column_count(), 2);
        assert_eq!(rowset.column_name(0), Some("id"));

        let chunk = rowset.next_chunk().unwrap().expect("one chunk");
        assert_eq!(chunk.len(), 100);
        assert!(rowset.next_chunk().unwrap().is_none());
    }

    #[test]
    fn test_streaming_rowset_multiple_streams() {
        // Four independent IPC streams of 500 rows each — the shape of
        // multi-chunk gRPC results. Total row count is verified.
        let chunks = serialize_independent_streams(4, 500);
        assert_eq!(chunks.len(), 4);
        let source = Box::new(VecChunkSource::new(chunks));
        let mut rowset = ArrowRowset::from_stream(source).unwrap();

        let mut total_rows = 0;
        while let Some(chunk) = rowset.next_chunk().unwrap() {
            total_rows += chunk.len();
        }
        assert_eq!(total_rows, 2000);
    }

    #[test]
    fn test_streaming_rowset_empty_source() {
        let source = Box::new(VecChunkSource::new(vec![]));
        let mut rowset = ArrowRowset::from_stream(source).unwrap();
        assert!(rowset.next_chunk().unwrap().is_none());
        assert!(rowset.is_empty());
    }

    #[test]
    fn test_from_bytes_concatenated_streams() {
        // Two self-contained IPC streams concatenated end to end — the
        // shape `GrpcQueryResult::into_arrow_data` produces when multiple
        // chunks are glued together.
        let streams = serialize_independent_streams(2, 300);
        let mut concat = bytes::BytesMut::new();
        for s in &streams {
            concat.extend_from_slice(s);
        }
        let rowset = ArrowRowset::from_bytes(concat.freeze()).unwrap();
        let mut total_rows = 0usize;
        let mut rowset = rowset;
        while let Some(chunk) = rowset.next_chunk().unwrap() {
            total_rows += chunk.len();
        }
        assert_eq!(total_rows, 600);
    }
}
