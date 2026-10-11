// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::sync::Arc;
use tokio::sync::Mutex;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::connection::Connection;
use crate::types::TableDefinition;

// =============================================================================
// ArrowInserter
// =============================================================================

/// Arrow-native bulk inserter.
///
/// Accepts a raw Arrow IPC stream (the byte buffer produced by
/// `apache-arrow`'s `RecordBatchStreamWriter` / `Table.serialize()`) and
/// streams it straight into Hyper via the async COPY path. There is no
/// per-row JS→Rust conversion — this is the fastest way to bulk-load
/// tabular data from a Node.js process that already has Arrow batches
/// in hand.
///
/// @example
/// ```js
/// import { tableFromArrays } from 'apache-arrow';
///
/// const table = tableFromArrays({
///   id: Int32Array.from([1, 2, 3]),
///   value: Float64Array.from([1.5, 2.5, 3.5]),
/// });
///
/// const inserter = await ArrowInserter.create(conn, tableDef);
/// await inserter.insertRaw(Buffer.from(table.serialize()));
/// const rowCount = await inserter.execute();
/// ```
#[napi]
#[derive(Debug)]
pub struct ArrowInserter {
    /// `Option` so `execute` / `cancel` can take ownership without
    /// consuming `self` (napi requires `&self` on instance methods).
    inner: Arc<Mutex<Option<hyperdb_api::AsyncArrowInserterOwned>>>,
}

fn already_executed() -> Error {
    Error::from_reason("ArrowInserter has already been executed or cancelled")
}

#[napi]
impl ArrowInserter {
    /// Creates a new `ArrowInserter` bound to the given connection and table.
    #[napi(factory)]
    pub fn create(connection: &Connection, table_def: &TableDefinition) -> Result<Self> {
        let inserter =
            hyperdb_api::AsyncArrowInserterOwned::new(connection.inner_arc(), &table_def.inner)
                .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(ArrowInserter {
            inner: Arc::new(Mutex::new(Some(inserter))),
        })
    }

    /// Sends a chunk of Arrow IPC stream bytes to the server.
    ///
    /// Accepts the raw byte buffer produced by `apache-arrow`'s
    /// `RecordBatchStreamWriter` — schema on the first call, record
    /// batches on subsequent calls, or a complete IPC stream in one
    /// shot.
    #[napi]
    pub async fn insert_raw(&self, buf: Buffer) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let ins = guard.as_mut().ok_or_else(already_executed)?;
        ins.insert_raw(&buf)
            .await
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Finalizes the COPY stream and returns the number of rows inserted.
    ///
    /// After this call, the inserter cannot be reused.
    #[napi]
    pub async fn execute(&self) -> Result<i64> {
        let mut guard = self.inner.lock().await;
        let ins = guard.take().ok_or_else(already_executed)?;
        ins.execute()
            .await
            .map_err(|e| Error::from_reason(e.to_string()))
            .and_then(|n| {
                i64::try_from(n).map_err(|_| Error::from_reason("row count exceeds i64::MAX"))
            })
    }

    /// Cancels the COPY stream without committing any rows.
    #[napi]
    pub async fn cancel(&self) -> Result<()> {
        let mut guard = self.inner.lock().await;
        if let Some(ins) = guard.take() {
            ins.cancel().await;
        }
        Ok(())
    }

    /// True until any data has been sent; useful for conditional flushes.
    #[napi]
    pub async fn has_data(&self) -> bool {
        let guard = self.inner.lock().await;
        guard
            .as_ref()
            .is_some_and(hyperdb_api::AsyncArrowInserterOwned::has_data)
    }

    /// Total bytes sent to the server so far.
    #[napi]
    pub async fn total_bytes(&self) -> u32 {
        let guard = self.inner.lock().await;
        guard
            .as_ref()
            .map_or(0, |i| u32::try_from(i.total_bytes()).unwrap_or(u32::MAX))
    }

    /// Number of Arrow IPC chunks sent so far.
    #[napi]
    pub async fn chunk_count(&self) -> u32 {
        let guard = self.inner.lock().await;
        guard
            .as_ref()
            .map_or(0, |i| u32::try_from(i.chunk_count()).unwrap_or(u32::MAX))
    }
}
