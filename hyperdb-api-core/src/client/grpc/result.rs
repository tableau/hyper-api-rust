// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! gRPC query result types.
//!
//! This module provides types for handling gRPC query results, which are
//! returned in Apache Arrow IPC format.

use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};

use crate::client::error::{Error, Result};

use super::proto::{QueryResultSchema, SqlType};

/// Result of a gRPC query execution.
///
/// Unlike TCP-based queries that return row-at-a-time results, gRPC queries
/// return results in Arrow IPC format, which can contain multiple record batches.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let config = GrpcConfig::new("http://localhost:7484");
/// # let mut client = GrpcClient::connect(config).await?;
/// let result = client.execute_query("SELECT * FROM users").await?;
///
/// // Get raw Arrow IPC bytes for all chunks
/// let all_arrow_data = result.arrow_data();
///
/// // Or process chunk by chunk
/// for chunk in result.chunks() {
///     let arrow_bytes = chunk.arrow_data();
///     // Process with arrow crate...
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct GrpcQueryResult {
    /// The query ID assigned by the server
    pub(crate) query_id: Option<String>,
    /// Schema information from the server
    pub(crate) schema: Option<QueryResultSchema>,
    /// Result chunks (Arrow IPC data)
    pub(crate) chunks: VecDeque<GrpcResultChunk>,
    /// Number of rows affected (for DML queries)
    pub(crate) rows_affected: Option<u64>,
    /// Whether the query is complete
    pub(crate) is_complete: bool,
}

impl GrpcQueryResult {
    /// Creates a new empty result.
    pub(crate) fn new() -> Self {
        GrpcQueryResult {
            query_id: None,
            schema: None,
            chunks: VecDeque::new(),
            rows_affected: None,
            is_complete: false,
        }
    }

    /// Returns the query ID assigned by the server, if available.
    #[must_use]
    pub fn query_id(&self) -> Option<&str> {
        self.query_id.as_deref()
    }

    /// Returns the number of columns in the result.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.schema.as_ref().map_or(0, |s| s.columns.len())
    }

    /// Returns the column descriptions from the server.
    pub fn columns(&self) -> impl Iterator<Item = GrpcColumnInfo<'_>> + '_ {
        self.schema
            .as_ref()
            .map(|s| s.columns.iter())
            .into_iter()
            .flatten()
            .map(|col| GrpcColumnInfo {
                name: &col.name,
                sql_type: col.r#type,
            })
    }

    /// Returns whether there are more result chunks available.
    #[must_use]
    pub fn has_chunks(&self) -> bool {
        !self.chunks.is_empty()
    }

    /// Takes the next result chunk, if available.
    pub fn take_chunk(&mut self) -> Option<GrpcResultChunk> {
        self.chunks.pop_front()
    }

    /// Returns an iterator over the result chunks.
    pub fn chunks(&self) -> impl Iterator<Item = &GrpcResultChunk> {
        self.chunks.iter()
    }

    /// Returns all Arrow IPC data concatenated.
    ///
    /// For queries with multiple chunks, this concatenates all Arrow record
    /// batches. Note that each chunk may have its own schema message, so the
    /// result may contain multiple schema messages.
    ///
    /// Single-chunk results are returned without any copy (refcount bump on
    /// the shared `Bytes`). Multi-chunk results are concatenated into a new
    /// `Bytes`. Prefer `chunk_bytes()` if you can process chunks incrementally.
    #[must_use]
    pub fn arrow_data(&self) -> Bytes {
        match self.chunks.len() {
            0 => Bytes::new(),
            1 => self.chunks[0].data.clone(),
            _ => {
                let total_len: usize = self.chunks.iter().map(|c| c.data.len()).sum();
                let mut buf = BytesMut::with_capacity(total_len);
                for chunk in &self.chunks {
                    buf.extend_from_slice(&chunk.data);
                }
                buf.freeze()
            }
        }
    }

    /// Consumes the result and returns all Arrow IPC data.
    ///
    /// Single-chunk results are returned without any copy. Multi-chunk
    /// results are concatenated into a new `Bytes`.
    #[must_use]
    pub fn into_arrow_data(mut self) -> Bytes {
        match self.chunks.len() {
            0 => Bytes::new(),
            1 => self.chunks.pop_front().map(|c| c.data).unwrap_or_default(),
            _ => {
                let total_len: usize = self.chunks.iter().map(|c| c.data.len()).sum();
                let mut buf = BytesMut::with_capacity(total_len);
                for chunk in self.chunks {
                    buf.extend_from_slice(&chunk.data);
                }
                buf.freeze()
            }
        }
    }

    /// Returns an iterator over the raw Arrow IPC byte chunks.
    ///
    /// Each chunk is a refcount-bumped `Bytes` sharing the original gRPC frame
    /// allocation — no copies are performed. This is the preferred way to feed
    /// results into an incremental Arrow IPC decoder.
    pub fn chunk_bytes(&self) -> impl Iterator<Item = Bytes> + '_ {
        self.chunks.iter().map(|c| c.data.clone())
    }

    /// Returns the number of rows affected by a DML query.
    ///
    /// For SELECT queries, this is `None`. For INSERT/UPDATE/DELETE queries,
    /// this returns the number of rows affected.
    #[must_use]
    pub fn rows_affected(&self) -> Option<u64> {
        self.rows_affected
    }

    /// Returns whether the query execution is complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.is_complete
    }
}

impl Default for GrpcQueryResult {
    fn default() -> Self {
        Self::new()
    }
}

/// A single chunk of gRPC query results.
///
/// Each chunk contains Arrow IPC data (schema + record batch) and metadata
/// about the chunk. The data is held as `Bytes`, so it shares the underlying
/// allocation with the gRPC frame it was decoded from — cloning the chunk or
/// extracting the bytes is a refcount bump, not a copy.
#[derive(Debug)]
pub struct GrpcResultChunk {
    /// The chunk ID (for async/adaptive transfer modes)
    pub(crate) chunk_id: u64,
    /// Arrow IPC data (may include schema + record batch)
    pub(crate) data: Bytes,
    /// Number of rows in this chunk
    pub(crate) row_count: Option<usize>,
}

impl GrpcResultChunk {
    /// Creates a new result chunk.
    pub(crate) fn new(chunk_id: u64, data: Bytes) -> Self {
        GrpcResultChunk {
            chunk_id,
            data,
            row_count: None,
        }
    }

    /// Returns the chunk ID.
    pub fn chunk_id(&self) -> u64 {
        self.chunk_id
    }

    /// Returns the Arrow IPC data for this chunk.
    pub fn arrow_data(&self) -> &[u8] {
        &self.data
    }

    /// Returns the Arrow IPC data as a shared `Bytes` handle.
    ///
    /// This is a refcount bump, not a copy — the returned `Bytes` shares the
    /// same allocation as the chunk.
    pub fn arrow_bytes(&self) -> Bytes {
        self.data.clone()
    }

    /// Consumes the chunk and returns the Arrow IPC data.
    pub fn into_arrow_data(self) -> Bytes {
        self.data
    }

    /// Returns the number of rows in this chunk, if known.
    pub fn row_count(&self) -> Option<usize> {
        self.row_count
    }

    /// Returns whether this chunk is empty (no data).
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Column information from a gRPC query result.
#[derive(Debug)]
pub struct GrpcColumnInfo<'a> {
    /// Column name
    pub(crate) name: &'a str,
    /// SQL type information (the generated protobuf message, so it is not
    /// part of the public API; see [`Self::type_name`])
    pub(crate) sql_type: Option<SqlType>,
}

impl GrpcColumnInfo<'_> {
    /// Returns the column name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.name
    }

    /// Returns the SQL type tag (e.g., "INTEGER", "TEXT").
    #[must_use]
    pub fn type_name(&self) -> Option<&'static str> {
        use crate::client::grpc::proto::hyper_service::sql_type::TypeTag;

        self.sql_type
            .as_ref()
            .and_then(|t| match TypeTag::try_from(t.tag).ok()? {
                TypeTag::HyperUnspecified => None,
                TypeTag::HyperBool => Some("BOOLEAN"),
                TypeTag::HyperBigInt => Some("BIGINT"),
                TypeTag::HyperSmallInt => Some("SMALLINT"),
                TypeTag::HyperInt => Some("INTEGER"),
                TypeTag::HyperNumeric => Some("NUMERIC"),
                TypeTag::HyperDouble => Some("DOUBLE PRECISION"),
                TypeTag::HyperFloat => Some("REAL"),
                TypeTag::HyperOid => Some("OID"),
                TypeTag::HyperByteA => Some("BYTEA"),
                TypeTag::HyperText => Some("TEXT"),
                TypeTag::HyperVarchar => Some("VARCHAR"),
                TypeTag::HyperChar => Some("CHAR"),
                TypeTag::HyperJson => Some("JSON"),
                TypeTag::HyperDate => Some("DATE"),
                TypeTag::HyperInterval => Some("INTERVAL"),
                TypeTag::HyperTime => Some("TIME"),
                TypeTag::HyperTimestamp => Some("TIMESTAMP"),
                TypeTag::HyperTimestampTz => Some("TIMESTAMPTZ"),
                TypeTag::HyperGeography => Some("GEOGRAPHY"),
                TypeTag::HyperArrayOfFloat => Some("FLOAT[]"),
            })
    }
}

/// Converts Hyper SQL types from the gRPC schema to hyper-types.
///
/// This is useful for applications that need to work with Hyper's type system
/// rather than Arrow's type system.
#[allow(
    dead_code,
    reason = "helper retained for callers that want to convert gRPC schema to hyper-types"
)]
pub(super) fn sql_type_to_hyper_type(sql_type: &SqlType) -> Result<crate::types::SqlType> {
    use super::proto::hyper_service::sql_type::{Modifier, TypeTag};
    use crate::types::SqlType as HyperSqlType;

    let tag = TypeTag::try_from(sql_type.tag)
        .map_err(|_| Error::conversion(format!("Unknown SQL type tag: {}", sql_type.tag)))?;

    // Extract modifier for types that need it
    let modifier = &sql_type.modifier;

    match tag {
        TypeTag::HyperUnspecified => Err(Error::conversion("Unspecified SQL type")),
        TypeTag::HyperBool => Ok(HyperSqlType::Bool),
        TypeTag::HyperSmallInt => Ok(HyperSqlType::SmallInt),
        TypeTag::HyperInt => Ok(HyperSqlType::Int),
        TypeTag::HyperBigInt => Ok(HyperSqlType::BigInt),
        TypeTag::HyperFloat => Ok(HyperSqlType::Float),
        TypeTag::HyperDouble => Ok(HyperSqlType::Double),
        TypeTag::HyperNumeric => {
            // Extract precision/scale from modifier
            let (precision, scale) = match modifier {
                Some(Modifier::NumericModifier(m)) => (m.precision, m.scale),
                _ => (38, 0), // Default precision/scale
            };
            Ok(HyperSqlType::Numeric { precision, scale })
        }
        TypeTag::HyperText => Ok(HyperSqlType::Text),
        TypeTag::HyperVarchar => {
            let max_length = match modifier {
                Some(Modifier::MaxLength(len)) => Some(*len),
                _ => None,
            };
            Ok(HyperSqlType::Varchar { max_length })
        }
        TypeTag::HyperChar => {
            let length = match modifier {
                Some(Modifier::MaxLength(len)) => *len,
                _ => 1,
            };
            Ok(HyperSqlType::Char { length })
        }
        TypeTag::HyperByteA => Ok(HyperSqlType::ByteA),
        TypeTag::HyperOid => Ok(HyperSqlType::Oid),
        TypeTag::HyperJson => Ok(HyperSqlType::Json),
        TypeTag::HyperDate => Ok(HyperSqlType::Date),
        TypeTag::HyperTime => Ok(HyperSqlType::Time),
        TypeTag::HyperTimestamp => Ok(HyperSqlType::Timestamp),
        TypeTag::HyperTimestampTz => Ok(HyperSqlType::TimestampTz),
        TypeTag::HyperInterval => Ok(HyperSqlType::Interval),
        TypeTag::HyperGeography => Ok(HyperSqlType::Geography),
        TypeTag::HyperArrayOfFloat => {
            // Array types are not directly supported in crate::types::SqlType
            Err(Error::conversion("Array types not yet supported"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_result_empty() {
        let result = GrpcQueryResult::new();
        assert!(!result.has_chunks());
        assert!(result.arrow_data().is_empty());
        assert_eq!(result.column_count(), 0);
    }

    #[test]
    fn test_grpc_result_with_chunks() {
        let mut result = GrpcQueryResult::new();
        result
            .chunks
            .push_back(GrpcResultChunk::new(0, Bytes::from_static(&[1, 2, 3])));
        result
            .chunks
            .push_back(GrpcResultChunk::new(1, Bytes::from_static(&[4, 5, 6])));

        assert!(result.has_chunks());
        assert_eq!(result.arrow_data().as_ref(), &[1, 2, 3, 4, 5, 6]);

        let chunk = result.take_chunk().unwrap();
        assert_eq!(chunk.chunk_id(), 0);
        assert_eq!(chunk.arrow_data(), &[1, 2, 3]);
    }

    #[test]
    fn test_sql_type_mapping() {
        use crate::client::grpc::proto::hyper_service::sql_type::TypeTag;

        let sql_type = SqlType {
            tag: TypeTag::HyperInt.into(),
            modifier: None,
        };

        let hyper_type = sql_type_to_hyper_type(&sql_type).unwrap();
        assert_eq!(hyper_type, crate::types::SqlType::Int);
    }
}
