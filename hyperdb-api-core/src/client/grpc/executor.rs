// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! gRPC query executor with state machine for result fetching.
//!
//! This module implements the query execution state machine that handles
//! the different transfer modes (SYNC, ASYNC, ADAPTIVE) and manages
//! fetching results from the Hyper gRPC service.
//!
//! # Transfer Mode State Machines
//!
//! Each transfer mode follows a different path through the executor states:
//!
//! **SYNC** — simplest path, all data in one response:
//! ```text
//! ReadInitialResults ──(stream exhausted)──> Finished
//! ```
//! The `ExecuteQuery` RPC returns a server-streaming response containing the
//! schema header followed by one or more binary/string data parts, then the
//! stream closes. Subject to the server's 100-second SYNC timeout.
//!
//! **ASYNC** — decouples submission from fetching:
//! ```text
//! ReadInitialResults ──(QueryStatus: Running)──> RequestStatus
//!     ──> ReadStatus ──(Running)──> RequestStatus  (poll loop)
//!     ──> ReadStatus ──(Finished)──> RequestResults
//!     ──> ReadResults ──(more chunks)──> RequestResults
//!     ──> ReadResults ──(all chunks)──> Finished
//! ```
//! The initial `ExecuteQuery` response contains only a `QueryStatus` with a
//! server-assigned `query_id`. The client polls `GetQueryInfo` until
//! `CompletionStatus::Finished`, then fetches result chunks via
//! `GetQueryResult` using chunk IDs.
//!
//! **ADAPTIVE** (default, recommended) — hybrid of SYNC and ASYNC:
//! ```text
//! ReadInitialResults ──(data + Finished)──> Finished       (small result)
//! ReadInitialResults ──(data + Running)──> RequestStatus   (large result)
//!     ──> ... (same as ASYNC from here)
//! ```
//! The first chunk of results is returned inline in the `ExecuteQuery`
//! response. If the query completes within that first chunk, the path is
//! identical to SYNC (no polling). If the result is larger, the response
//! includes a `QueryStatus` with `Running` and the executor transitions
//! to the ASYNC polling path for remaining chunks.

use bytes::Bytes;
use tonic::Streaming;
use tracing::{debug, trace, warn};

use crate::client::error::{Error, Result};

use super::config::TransferMode;
use super::error::from_grpc_status;
use super::proto::hyper_service::query_result::Result as QueryResultPayload;
use super::proto::hyper_service::query_status::CompletionStatus;
use super::proto::{
    ExecuteQueryResponse, HyperServiceClient, QueryInfo, QueryInfoParam, QueryResult,
    QueryResultParam, QueryStatus,
};
use super::result::{GrpcQueryResult, GrpcResultChunk};

/// State of the query executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutorState {
    /// Reading initial results from `ExecuteQuery` stream
    ReadInitialResults,
    /// Requesting query status via `GetQueryInfo`
    RequestStatus,
    /// Reading query status
    ReadStatus,
    /// Requesting result chunks via `GetQueryResult`
    RequestResults,
    /// Reading result chunks
    ReadResults,
    /// Query execution complete
    Finished,
}

/// Executes gRPC queries and manages result fetching.
///
/// This mirrors the C++ `GrpcQueryExecutor` implementation, handling the
/// different transfer modes and async result fetching.
pub(crate) struct GrpcQueryExecutor<T> {
    /// The gRPC client
    client: HyperServiceClient<T>,
    /// Metadata headers for requests
    headers: Vec<(String, String)>,
    /// Current state of the executor
    state: ExecutorState,
    /// Transfer mode being used
    transfer_mode: TransferMode,
    /// Stream for `ExecuteQuery` responses
    execute_stream: Option<Streaming<ExecuteQueryResponse>>,
    /// Stream for `GetQueryInfo` responses
    query_info_stream: Option<Streaming<QueryInfo>>,
    /// Stream for `GetQueryResult` responses
    query_result_stream: Option<Streaming<QueryResult>>,
    /// Query status from server
    query_status: Option<QueryStatus>,
    /// Query ID for async operations
    query_id: Option<String>,
    /// Monotonic label for the next `GrpcResultChunk` appended to
    /// `self.result.chunks`. Bumped once per received `QueryResult` /
    /// `BinaryPart` / `StringPart` message. This is *purely* a local
    /// identifier for downstream consumers and has no relationship to
    /// server-side chunk IDs.
    next_local_chunk_id: u64,
    /// Server-side chunk ID to request in the next `GetQueryResult` RPC,
    /// and the value compared against `QueryStatus.chunk_count` to decide
    /// when all chunks have been fetched.
    ///
    /// Bumped by exactly 1 after each `GetQueryResult` stream is fully
    /// drained — regardless of how many `QueryResult` messages that stream
    /// contained. Mirrors `nextChunkId_` in the C++ `GrpcQueryExecutor`.
    ///
    /// Initial value depends on transfer mode:
    /// - `ASYNC`: `0` — no chunks delivered inline.
    /// - `ADAPTIVE`: `1` — server sends chunk 0 inline on `ExecuteQuery`.
    /// - `SYNC`: unused (all data arrives via `ExecuteQuery`).
    next_server_chunk_id: u64,
    /// Result being built
    result: GrpcQueryResult,
}

impl<T> GrpcQueryExecutor<T>
where
    T: tonic::client::GrpcService<tonic::body::Body> + Clone + Send + 'static,
    T::ResponseBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error:
        Into<tonic::codegen::StdError> + Send + 'static,
    T::Future: Send,
{
    /// Creates a new query executor.
    pub(crate) fn new(
        client: HyperServiceClient<T>,
        headers: Vec<(String, String)>,
        transfer_mode: TransferMode,
    ) -> Self {
        // Match the C++ `GrpcQueryExecutor` constructor: ADAPTIVE delivers
        // server-side chunk 0 inline on the `ExecuteQuery` response, so the
        // first chunk we need to request via `GetQueryResult` is chunk 1.
        // ASYNC doesn't deliver any chunks inline, so we start at 0. SYNC
        // never reaches the GetQueryResult state machine.
        let next_server_chunk_id = match transfer_mode {
            TransferMode::Adaptive => 1,
            _ => 0,
        };
        GrpcQueryExecutor {
            client,
            headers,
            state: ExecutorState::ReadInitialResults,
            transfer_mode,
            execute_stream: None,
            query_info_stream: None,
            query_result_stream: None,
            query_status: None,
            query_id: None,
            next_local_chunk_id: 0,
            next_server_chunk_id,
            result: GrpcQueryResult::new(),
        }
    }

    /// Starts query execution.
    pub(crate) async fn execute(&mut self, query: super::proto::QueryParam) -> Result<()> {
        debug!(query = %query.query, transfer_mode = ?self.transfer_mode, "Executing gRPC query");

        // Create request with metadata
        let mut request = tonic::Request::new(query);
        for (key, value) in &self.headers {
            if let (Ok(key), Ok(value)) = (
                key.parse::<tonic::metadata::MetadataKey<_>>(),
                value.parse(),
            ) {
                request.metadata_mut().insert(key, value);
            }
        }

        // Execute the query
        let response = self
            .client
            .execute_query(request)
            .await
            .map_err(from_grpc_status)?;

        self.execute_stream = Some(response.into_inner());
        self.state = ExecutorState::ReadInitialResults;

        Ok(())
    }

    /// Gets the next result.
    ///
    /// Returns `None` when all results have been consumed.
    pub(crate) async fn next_result(&mut self) -> Result<Option<GrpcQueryResult>> {
        loop {
            trace!(state = ?self.state, "Query executor state");

            match self.state {
                ExecutorState::ReadInitialResults => {
                    self.read_initial_results().await?;
                }
                ExecutorState::RequestStatus => {
                    self.request_status().await?;
                }
                ExecutorState::ReadStatus => {
                    self.read_status().await?;
                }
                ExecutorState::RequestResults => {
                    self.request_results().await?;
                }
                ExecutorState::ReadResults => {
                    self.read_results().await?;
                }
                ExecutorState::Finished => {
                    self.result.is_complete = true;
                    // Return the accumulated result
                    return Ok(Some(std::mem::take(&mut self.result)));
                }
            }

            // Yield back to the caller as soon as we either have some
            // chunks to deliver or have reached the terminal state.
            // Streaming out of `ReadInitialResults` keeps peak memory
            // bounded for SYNC/inline paths (otherwise we would buffer
            // the entire ExecuteQuery stream before the first yield).
            if self.state == ExecutorState::Finished || !self.result.chunks.is_empty() {
                break;
            }
        }

        if self.result.is_complete || !self.result.chunks.is_empty() {
            Ok(Some(std::mem::take(&mut self.result)))
        } else {
            Ok(None)
        }
    }

    /// Reads one message from the `ExecuteQuery` stream.
    ///
    /// We deliberately do **not** transition state the moment we see a
    /// `QueryStatus` — under `ADAPTIVE` the server delivers the whole of
    /// chunk 0 inline *followed by* a `QueryStatus(Running)` and then
    /// closes the stream, so bailing early would silently drop the tail
    /// of chunk 0. Only the server-side close (a `None` message) decides
    /// where to go next. This mirrors the C++ `GrpcQueryExecutor::
    /// READ_INITIAL_RESULTS` loop, which reads until `Read()` returns
    /// false and only then inspects `queryStatus_` to pick the next
    /// state.
    ///
    /// One message per call keeps memory bounded: the outer `next_result`
    /// loop yields accumulated chunks back to the caller as they arrive
    /// instead of buffering the entire inline response.
    async fn read_initial_results(&mut self) -> Result<()> {
        let response = {
            let stream = self
                .execute_stream
                .as_mut()
                .ok_or_else(|| Error::protocol("ExecuteQuery stream not initialized"))?;
            stream.message().await.map_err(from_grpc_status)?
        };

        match response {
            Some(response) => {
                self.process_execute_response(response)?;
            }
            None => {
                // Server closed the stream. Where we go next is determined
                // purely by transfer mode, mirroring the C++
                // `GrpcQueryExecutor`:
                //   - SYNC: all data is inline, we're done.
                //   - ASYNC / ADAPTIVE: go to the GetQueryInfo/
                //     GetQueryResult state machine. A `QueryStatus` of
                //     `Finished` here means *query execution* is finished,
                //     NOT that all chunks have been streamed — server-side
                //     chunks 1..N still need to be fetched for ADAPTIVE
                //     (and 0..N for ASYNC).
                match self.transfer_mode {
                    TransferMode::Sync => {
                        debug!(
                            query_id = ?self.query_id,
                            "ExecuteQuery stream closed; SYNC mode complete",
                        );
                        self.state = ExecutorState::Finished;
                    }
                    TransferMode::Async | TransferMode::Adaptive => {
                        debug!(
                            query_id = ?self.query_id,
                            mode = ?self.transfer_mode,
                            next_chunk = self.next_server_chunk_id,
                            "ExecuteQuery stream closed; fetching remaining chunks",
                        );
                        self.state = ExecutorState::RequestStatus;
                    }
                }
            }
        }

        Ok(())
    }

    /// Processes an `ExecuteQueryResponse` message.
    fn process_execute_response(&mut self, response: ExecuteQueryResponse) -> Result<()> {
        use super::proto::hyper_service::execute_query_response::Result as ResponsePayload;
        use super::proto::hyper_service::query_info::Content as QueryInfoContent;
        use super::proto::hyper_service::query_result_header::Header;

        match response.result {
            Some(ResponsePayload::Header(header)) => match header.header {
                Some(Header::Schema(schema)) => {
                    debug!(columns = schema.columns.len(), "Received schema");
                    self.result.schema = Some(schema);
                }
                Some(Header::Command(cmd)) => {
                    use super::proto::hyper_service::query_command_ok::CommandReturn;
                    let rows = match cmd.command_return {
                        Some(CommandReturn::AffectedRows(n)) => Some(n),
                        Some(CommandReturn::Empty(())) | None => None,
                    };
                    debug!(rows_affected = ?rows, "Command OK");
                    self.result.rows_affected = rows;
                    self.state = ExecutorState::Finished;
                }
                None => {
                    warn!("Received empty QueryResultHeader");
                }
            },
            Some(ResponsePayload::BinaryPart(data)) => {
                debug!(bytes = data.data.len(), "Received binary result part");
                let chunk = GrpcResultChunk::new(self.next_local_chunk_id, data.data);
                self.next_local_chunk_id += 1;
                self.result.chunks.push_back(chunk);
            }
            Some(ResponsePayload::StringPart(data)) => {
                debug!(len = data.data.len(), "Received string result part");
                let chunk = GrpcResultChunk::new(
                    self.next_local_chunk_id,
                    Bytes::from(data.data.into_bytes()),
                );
                self.next_local_chunk_id += 1;
                self.result.chunks.push_back(chunk);
            }
            Some(ResponsePayload::QueryInfo(info)) => {
                match info.content {
                    Some(QueryInfoContent::QueryStatus(status)) => {
                        self.process_query_status(status);
                    }
                    Some(QueryInfoContent::BinarySchema(data)) => {
                        debug!(bytes = data.data.len(), "Received binary schema");
                        // Schema in binary form - store as a chunk
                        let chunk = GrpcResultChunk::new(self.next_local_chunk_id, data.data);
                        self.next_local_chunk_id += 1;
                        self.result.chunks.push_back(chunk);
                    }
                    Some(QueryInfoContent::StringSchema(data)) => {
                        debug!(len = data.data.len(), "Received string schema");
                        // Schema in string form - for JSON format
                        let chunk = GrpcResultChunk::new(
                            self.next_local_chunk_id,
                            Bytes::from(data.data.into_bytes()),
                        );
                        self.next_local_chunk_id += 1;
                        self.result.chunks.push_back(chunk);
                    }
                    None => {}
                }
            }
            Some(ResponsePayload::QueryResult(query_result)) => {
                self.process_query_result(query_result)?;
            }
            None => {
                warn!("Received empty ExecuteQueryResponse");
            }
        }
        Ok(())
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "signature retained for API symmetry / future fallibility; returning Result/Option keeps callers from breaking when the function later grows failure cases"
    )]
    /// Processes a `QueryResult` message.
    ///
    /// Note: a single `GetQueryResult` RPC returns *multiple* `QueryResult`
    /// messages for one server-side chunk (schema + N binary parts). Only
    /// the local `GrpcResultChunk` label is bumped here — the server-side
    /// chunk ID (`next_server_chunk_id`) is advanced by exactly 1 after the
    /// RPC stream has been fully drained; see `read_results`.
    fn process_query_result(&mut self, result: QueryResult) -> Result<()> {
        // Extract data payload
        if let Some(payload) = result.result {
            let chunk = match payload {
                QueryResultPayload::BinaryPart(data) => {
                    debug!(bytes = data.data.len(), "Received binary result chunk");
                    GrpcResultChunk::new(self.next_local_chunk_id, data.data)
                }
                QueryResultPayload::StringPart(data) => {
                    // Convert string data to bytes
                    debug!(len = data.data.len(), "Received string result chunk");
                    GrpcResultChunk::new(
                        self.next_local_chunk_id,
                        Bytes::from(data.data.into_bytes()),
                    )
                }
            };
            self.next_local_chunk_id += 1;
            self.result.chunks.push_back(chunk);
        }

        Ok(())
    }

    /// Processes a `QueryStatus` message.
    fn process_query_status(&mut self, status: QueryStatus) {
        debug!(
            query_id = %status.query_id,
            completion_status = ?CompletionStatus::try_from(status.completion_status),
            "Received query status"
        );

        self.query_id = Some(status.query_id.clone());
        self.result.query_id = Some(status.query_id.clone());
        self.query_status = Some(status);
    }

    /// Requests query status via `GetQueryInfo`.
    async fn request_status(&mut self) -> Result<()> {
        let query_id = self
            .query_id
            .clone()
            .ok_or_else(|| Error::protocol("No query ID for status request"))?;

        debug!(query_id = %query_id, "Requesting query status");

        let param = QueryInfoParam {
            query_id: query_id.clone(),
            streaming: true,         // Enable streaming to get continuous updates
            schema_output_format: 0, // OUTPUT_FORMAT_UNSPECIFIED - we don't need schema here
        };

        let mut request = tonic::Request::new(param);
        // Add the required x-hyperdb-query-id header
        if let Ok(value) = query_id.parse() {
            request.metadata_mut().insert("x-hyperdb-query-id", value);
        }
        for (key, value) in &self.headers {
            if let (Ok(key), Ok(value)) = (
                key.parse::<tonic::metadata::MetadataKey<_>>(),
                value.parse(),
            ) {
                request.metadata_mut().insert(key, value);
            }
        }

        let response = self
            .client
            .get_query_info(request)
            .await
            .map_err(from_grpc_status)?;

        self.query_info_stream = Some(response.into_inner());
        self.state = ExecutorState::ReadStatus;
        Ok(())
    }

    /// Reads query status from `GetQueryInfo` stream.
    async fn read_status(&mut self) -> Result<()> {
        use super::proto::hyper_service::query_info::Content as QueryInfoContent;

        let stream = self
            .query_info_stream
            .as_mut()
            .ok_or_else(|| Error::protocol("QueryInfo stream not initialized"))?;

        if let Some(info) = stream.message().await.map_err(from_grpc_status)? {
            match info.content {
                Some(QueryInfoContent::QueryStatus(status)) => {
                    self.process_query_status(status.clone());

                    match CompletionStatus::try_from(status.completion_status)
                        .unwrap_or(CompletionStatus::RunningOrUnspecified)
                    {
                        CompletionStatus::Finished | CompletionStatus::ResultsProduced => {
                            debug!("Query finished, requesting results");
                            self.state = ExecutorState::RequestResults;
                        }
                        CompletionStatus::RunningOrUnspecified => {
                            // Keep polling
                            self.state = ExecutorState::RequestStatus;
                        }
                    }
                }
                Some(QueryInfoContent::BinarySchema(_) | QueryInfoContent::StringSchema(_)) => {
                    // Schema received - just continue polling
                    self.state = ExecutorState::RequestStatus;
                }
                None => {
                    self.state = ExecutorState::RequestStatus;
                }
            }
        }

        Ok(())
    }

    /// Requests result chunks via `GetQueryResult`.
    async fn request_results(&mut self) -> Result<()> {
        use super::proto::hyper_service::query_result_param::RequestedData;

        // Short-circuit when we've already fetched every chunk the server
        // reported. This matches C++ `GrpcQueryExecutor`: don't emit a
        // `GetQueryResult(chunk_id=k)` when `k >= chunk_count`. Otherwise
        // the server returns error code 22023 ("chunk id out of range").
        //
        // Common case: ADAPTIVE with a small result that fit entirely in
        // chunk 0 (delivered inline). `next_server_chunk_id` starts at 1
        // and `chunk_count` is 1, so we skip straight to Finished.
        if let Some(ref status) = self.query_status
            && status.chunk_count > 0
            && self.next_server_chunk_id >= status.chunk_count
        {
            debug!(
                total_chunks = status.chunk_count,
                next_chunk = self.next_server_chunk_id,
                "No more chunks to fetch",
            );
            self.state = ExecutorState::Finished;
            return Ok(());
        }

        let query_id = self
            .query_id
            .clone()
            .ok_or_else(|| Error::protocol("No query ID for result request"))?;

        debug!(
            query_id = %query_id,
            chunk_id = self.next_server_chunk_id,
            "Requesting result chunks"
        );

        let param = QueryResultParam {
            query_id: query_id.clone(),
            output_format: super::proto::OutputFormat::ArrowIpc.into(),
            requested_data: Some(RequestedData::ChunkId(self.next_server_chunk_id)),
            // The schema is delivered inline on the initial `ExecuteQuery`
            // stream for both ASYNC and ADAPTIVE (hyperd sends a
            // `QueryInfo.binary_schema` message before closing that
            // stream). Asking for it again on every `GetQueryResult`
            // would emit extra schema frames that confuse an incremental
            // Arrow IPC decoder. C++ `GrpcQueryExecutor` also sets this
            // to `true`.
            omit_schema: true,
        };

        let mut request = tonic::Request::new(param);
        // Add the required x-hyperdb-query-id header
        if let Ok(value) = query_id.parse() {
            request.metadata_mut().insert("x-hyperdb-query-id", value);
        }
        for (key, value) in &self.headers {
            if let (Ok(key), Ok(value)) = (
                key.parse::<tonic::metadata::MetadataKey<_>>(),
                value.parse(),
            ) {
                request.metadata_mut().insert(key, value);
            }
        }

        let response = self
            .client
            .get_query_result(request)
            .await
            .map_err(from_grpc_status)?;

        self.query_result_stream = Some(response.into_inner());
        self.state = ExecutorState::ReadResults;
        Ok(())
    }

    /// Reads result chunks from `GetQueryResult` stream.
    async fn read_results(&mut self) -> Result<()> {
        loop {
            let result = {
                let stream = self
                    .query_result_stream
                    .as_mut()
                    .ok_or_else(|| Error::protocol("QueryResult stream not initialized"))?;
                stream.message().await.map_err(from_grpc_status)?
            };

            match result {
                Some(result) => {
                    self.process_query_result(result)?;
                }
                None => break,
            }
        }

        // One GetQueryResult RPC corresponds to exactly one server-side
        // chunk, regardless of how many QueryResult messages it contained
        // on the wire. Advance by 1 (matches C++ GrpcQueryExecutor).
        self.next_server_chunk_id += 1;

        // Check if there are more chunks
        if let Some(ref status) = self.query_status {
            let total_chunks = status.chunk_count;
            if self.next_server_chunk_id >= total_chunks {
                debug!(total_chunks, "All chunks received");
                self.state = ExecutorState::Finished;
            } else {
                debug!(
                    next_chunk = self.next_server_chunk_id,
                    total_chunks, "More chunks available"
                );
                self.state = ExecutorState::RequestResults;
            }
        } else {
            self.state = ExecutorState::Finished;
        }

        Ok(())
    }
}

// ============================================================================
// Streaming chunk producer
// ============================================================================

/// Streaming producer of Arrow IPC byte chunks from a gRPC query.
///
/// Unlike [`GrpcClient::execute_query`][`super::client::GrpcClient::execute_query`],
/// which drains every result chunk into a single [`GrpcQueryResult`] before
/// returning, this type yields one [`Bytes`] chunk at a time. The caller can
/// decode each chunk (e.g. via `arrow_ipc::reader::StreamDecoder`) and drop
/// it before fetching the next, so memory stays bounded by roughly one
/// message (capped at the tonic `max_decoding_message_size`, default 64 MB)
/// regardless of total result size.
///
/// Built by [`GrpcClient::execute_query_stream`][`super::client::GrpcClient::execute_query_stream`]
/// and the `AuthenticatedGrpcClient` variant.
pub struct GrpcChunkStream {
    executor: GrpcQueryExecutor<tonic::transport::Channel>,
    pending: std::collections::VecDeque<bytes::Bytes>,
    schema: Option<super::proto::QueryResultSchema>,
    query_id: Option<String>,
    rows_affected: Option<u64>,
    done: bool,
}

impl std::fmt::Debug for GrpcChunkStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcChunkStream")
            .field("pending_chunks", &self.pending.len())
            .field("query_id", &self.query_id)
            .field("rows_affected", &self.rows_affected)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl GrpcChunkStream {
    pub(crate) fn new(executor: GrpcQueryExecutor<tonic::transport::Channel>) -> Self {
        GrpcChunkStream {
            executor,
            pending: std::collections::VecDeque::new(),
            schema: None,
            query_id: None,
            rows_affected: None,
            done: false,
        }
    }

    /// Returns the next Arrow IPC byte chunk from the stream, or `None` when
    /// the server has signalled that the stream is complete.
    ///
    /// # Errors
    ///
    /// Propagates any error from the underlying executor's
    /// `next_result` call — typically [`tonic::Status`] errors wrapped
    /// as [`Error`] (server-side query failure, auth expiry, or
    /// transport-level gRPC errors).
    pub async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>> {
        loop {
            if let Some(b) = self.pending.pop_front() {
                return Ok(Some(b));
            }
            if self.done {
                return Ok(None);
            }
            match self.executor.next_result().await? {
                Some(mut partial) => {
                    if self.schema.is_none() {
                        self.schema = partial.schema.take();
                    }
                    if self.query_id.is_none() {
                        self.query_id = partial.query_id.take();
                    }
                    if partial.rows_affected.is_some() {
                        self.rows_affected = partial.rows_affected;
                    }
                    while let Some(chunk) = partial.take_chunk() {
                        self.pending.push_back(chunk.data);
                    }
                    if partial.is_complete {
                        self.done = true;
                    }
                }
                None => {
                    self.done = true;
                }
            }
        }
    }

    /// Returns the schema reported by the server for this query, if one has
    /// been received yet.
    ///
    /// The schema is typically delivered as the first message on the stream,
    /// so it is usually available after the first `next_chunk()` call.
    pub fn schema(&self) -> Option<&super::proto::QueryResultSchema> {
        self.schema.as_ref()
    }

    /// Returns the server-assigned query ID, if one has been received.
    pub fn query_id(&self) -> Option<&str> {
        self.query_id.as_deref()
    }

    /// Returns the affected row count for DML queries, if reported.
    pub fn rows_affected(&self) -> Option<u64> {
        self.rows_affected
    }
}
