// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Performance telemetry attached to every MCP tool response.
//!
//! Each stats struct captures the raw counters (rows, bytes, elapsed time) and
//! derives throughput metrics on the fly. The [`to_json`](IngestStats::to_json)
//! methods merge the derived fields into the serialized output so callers get a
//! single self-contained stats object.

#![allow(
    clippy::cast_precision_loss,
    reason = "diagnostic rate/throughput calculations displayed to user; >2^53 bytes is unreachable"
)]

use serde::Serialize;
use std::time::Instant;

/// Lightweight wall-clock timer used throughout ingest/query/export paths.
#[derive(Debug)]
pub struct StatsTimer {
    start: Instant,
}

impl StatsTimer {
    #[must_use]
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        // `Duration::as_millis` returns `u128`; `as u64` would
        // silently wrap at ~584 million years, which is absurd in
        // practice but banned by AGENTS.md reminder 7 (no narrowing integer
        // casts) because the cast is a latent data-corruption
        // vector if reached. `u64::try_from` with a saturating
        // fallback makes the overflow handling explicit.
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Telemetry for data ingest operations (`load_data`, `load_file`, one-shot ingest).
///
/// The `to_json()` method enriches the base fields with computed throughput:
/// `rows_per_sec`, `ingest_throughput_mb_sec`, and `compression_ratio`.
#[derive(Debug, Clone, Serialize)]
pub struct IngestStats {
    pub operation: String,
    /// Rows written to the target table by this call.
    ///
    /// For `replace` / `append` this matches the file row count. For
    /// `merge` it's the count returned by the post-DELETE INSERT —
    /// treat as "rows written to target" rather than "rows in input".
    pub rows: u64,
    pub elapsed_ms: u64,
    /// Raw input size (string length for inline data, file size for file ingest).
    pub bytes_read: u64,
    /// On-disk Hyper storage consumed. Zero when not measured.
    pub bytes_stored: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_inference_ms: Option<u64>,
    pub table: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_format: Option<String>,
    /// Advisory warning for the LLM (e.g. "use `load_file` for large inline data").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// `true` when this ingest changed the target's column set —
    /// today, only `mode = "merge"` can flip this on (via auto-
    /// `ALTER TABLE ADD COLUMN`). Drives whether the server fires a
    /// resource-list-changed notification post-call. Default `false`
    /// so non-merge paths don't have to think about it.
    #[serde(skip_serializing_if = "is_false")]
    #[serde(default)]
    pub schema_changed: bool,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires `&T -> bool`"
)]
fn is_false(b: &bool) -> bool {
    !*b
}

impl IngestStats {
    /// Rows ingested per second. Returns `rows` directly if elapsed is zero
    /// to avoid division by zero.
    #[must_use]
    pub fn rows_per_sec(&self) -> u64 {
        if self.elapsed_ms == 0 {
            return self.rows;
        }
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "diagnostic rate: operands are non-negative u64 throughput counters; Rust's f64→u64 cast saturates at u64::MAX, which is strictly better than overflow for a reported rate"
        )]
        let rate = (self.rows as f64 / (self.elapsed_ms as f64 / 1000.0)) as u64;
        rate
    }

    /// Input data throughput in MB/s (decimal megabytes).
    #[must_use]
    pub fn ingest_throughput_mb_sec(&self) -> f64 {
        if self.elapsed_ms == 0 {
            return 0.0;
        }
        (self.bytes_read as f64 / 1_000_000.0) / (self.elapsed_ms as f64 / 1000.0)
    }

    /// Ratio of stored bytes to input bytes. Values < 1.0 indicate Hyper's
    /// columnar compression is smaller than the source format.
    #[must_use]
    pub fn compression_ratio(&self) -> f64 {
        if self.bytes_read == 0 {
            return 1.0;
        }
        self.bytes_stored as f64 / self.bytes_read as f64
    }

    /// Serialize to JSON with derived throughput fields merged in.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("rows_per_sec".into(), self.rows_per_sec().into());
            obj.insert(
                "ingest_throughput_mb_sec".into(),
                serde_json::json!(self.ingest_throughput_mb_sec()),
            );
            obj.insert(
                "compression_ratio".into(),
                serde_json::json!(self.compression_ratio()),
            );
        }
        v
    }
}

/// Telemetry for SQL query operations.
#[derive(Debug, Clone, Serialize)]
pub struct QueryStats {
    pub operation: String,
    pub rows_returned: u64,
    /// Total rows scanned by the engine (zero when not available from Hyper).
    pub rows_scanned: u64,
    pub elapsed_ms: u64,
    /// Approximate size of the JSON result payload.
    pub result_size_bytes: u64,
    pub tables_touched: Vec<String>,
}

impl QueryStats {
    /// Scan throughput in rows/sec. Useful for gauging query selectivity.
    #[must_use]
    pub fn scan_rate_rows_sec(&self) -> u64 {
        if self.elapsed_ms == 0 {
            return self.rows_scanned;
        }
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "diagnostic rate: operands are non-negative u64 counters; Rust's f64→u64 cast saturates at u64::MAX, which is strictly better than overflow for a reported rate"
        )]
        let rate = (self.rows_scanned as f64 / (self.elapsed_ms as f64 / 1000.0)) as u64;
        rate
    }

    /// Serialize to JSON with `scan_rate_rows_sec` merged in.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "scan_rate_rows_sec".into(),
                self.scan_rate_rows_sec().into(),
            );
        }
        v
    }
}

/// Telemetry for export operations (CSV, Parquet, Arrow IPC, `.hyper`).
#[derive(Debug, Clone, Serialize)]
pub struct ExportStats {
    pub operation: String,
    pub rows: u64,
    pub elapsed_ms: u64,
    pub file_size_bytes: u64,
    pub format: String,
    pub output_path: String,
}

impl ExportStats {
    /// Export throughput in rows/sec.
    #[must_use]
    pub fn rows_per_sec(&self) -> u64 {
        if self.elapsed_ms == 0 {
            return self.rows;
        }
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "diagnostic rate: operands are non-negative u64 counters; Rust's f64→u64 cast saturates at u64::MAX, which is strictly better than overflow for a reported rate"
        )]
        let rate = (self.rows as f64 / (self.elapsed_ms as f64 / 1000.0)) as u64;
        rate
    }

    /// Serialize to JSON with `rows_per_sec` merged in.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("rows_per_sec".into(), self.rows_per_sec().into());
        }
        v
    }
}
