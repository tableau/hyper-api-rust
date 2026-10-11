// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Query statistics collection for Hyper database queries.
//!
//! Statistics are collected through the [`QueryStatsProvider`] trait.
//! [`LogFileStatsProvider`], the built-in implementation, parses Hyper's JSON log file
//! (`hyperd.log`) to extract per-query statistics. See [`QueryStats`] for usage.

use std::any::Any;
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::{debug, trace};

// =============================================================================
// Data Model
// =============================================================================

/// Detailed statistics for a single query execution.
///
/// Covers parsing time, compilation time, execution time, memory usage,
/// storage I/O, and plan cache status. All time fields are in seconds. Memory
/// fields are in megabytes. Fields are `Option` because not all queries produce
/// all stats (e.g., a simple `SET` command won't have execution storage stats).
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};
/// use hyperdb_api::LogFileStatsProvider;
///
/// fn main() -> Result<()> {
///     let hyper = HyperProcess::new(None, None)?;
///     let mut conn = Connection::new(&hyper, "test.hyper", CreateMode::CreateIfNotExists)?;
///
///     // Enable stats collection (auto-detect log path from HyperProcess)
///     conn.enable_query_stats(LogFileStatsProvider::from_process(&hyper));
///
///     // Execute a query
///     conn.execute_command("CREATE TABLE t (id INT)")?;
///
///     // Retrieve stats for the last query
///     if let Some(stats) = conn.last_query_stats() {
///         println!("Total elapsed: {}s", stats.elapsed_s);
///         if let Some(ref pre) = stats.pre_execution {
///             println!("  Parse: {:?}s, Compile: {:?}s",
///                 pre.parsing_time_s, pre.compilation_time_s);
///         }
///     }
///
///     Ok(())
/// }
/// ```
///
/// # Availability
///
/// - **Local `HyperProcess`**: Full stats available via log file parsing.
/// - **Remote standalone Hyper**: Not available with [`LogFileStatsProvider`], which needs a
///   readable local log file.
#[derive(Debug, Clone, Default)]
pub struct QueryStats {
    /// Total elapsed wall-clock time for the query (seconds); `0.0` if the log
    /// entry has no `elapsed` field.
    pub elapsed_s: f64,
    /// Time spent committing the transaction (seconds).
    pub commit_time_s: Option<f64>,
    /// Time waiting to be scheduled on a worker thread (seconds).
    pub time_to_schedule_s: Option<f64>,
    /// Pre-execution phase stats (parsing, compilation).
    pub pre_execution: Option<PreExecutionStats>,
    /// Execution phase stats (runtime, CPU, storage I/O).
    pub execution: Option<ExecutionStats>,
    /// Size of the result set sent to the client (MB).
    pub result_size_mb: Option<f64>,
    /// Peak memory used by the result buffer (MB).
    pub peak_result_buffer_memory_mb: Option<f64>,
    /// Plan cache status: "cache miss", "cache hit", "not run yet", etc.
    pub plan_cache_status: Option<String>,
    /// Number of times the cached plan was reused.
    pub plan_cache_hit_count: Option<u32>,
    /// Statement type: "SELECT", "INSERT", "SET", "ATTACH", "PREPARE", etc.
    pub statement_type: Option<String>,
    /// Number of result rows.
    pub rows: Option<u64>,
    /// Number of result columns.
    pub cols: Option<u32>,
    /// Truncated query text (as logged by Hyper).
    pub query_truncated: Option<String>,
}

/// Pre-execution phase statistics (parsing and compilation).
#[derive(Debug, Clone, Default)]
pub struct PreExecutionStats {
    /// Time spent parsing the SQL (seconds).
    pub parsing_time_s: Option<f64>,
    /// Time spent compiling the query plan (seconds).
    pub compilation_time_s: Option<f64>,
    /// Total pre-execution elapsed time (seconds).
    pub elapsed_s: Option<f64>,
    /// Peak transaction memory during pre-execution (MB).
    pub peak_memory_mb: Option<f64>,
}

/// Execution phase statistics (runtime performance).
#[derive(Debug, Clone, Default)]
pub struct ExecutionStats {
    /// Total execution elapsed time (seconds).
    pub elapsed_s: Option<f64>,
    /// CPU time consumed (seconds).
    pub cpu_time_s: Option<f64>,
    /// Total thread time (seconds).
    pub thread_time_s: Option<f64>,
    /// Time spent waiting (seconds).
    pub wait_time_s: Option<f64>,
    /// Total rows processed (including federated sources).
    pub processed_rows_total: Option<u64>,
    /// Rows processed from native (local) storage.
    pub processed_rows_native: Option<u64>,
    /// Time spent on storage access (seconds).
    pub storage_access_time_s: Option<f64>,
    /// Number of storage access operations.
    pub storage_access_count: Option<u64>,
    /// Bytes read from storage.
    pub storage_access_bytes: Option<u64>,
    /// Peak transaction memory during execution (MB).
    pub peak_memory_mb: Option<f64>,
}

// =============================================================================
// Provider Trait
// =============================================================================

/// Trait for collecting query statistics from a Hyper server.
///
/// Implementations capture stats from a source such as Hyper's log file. The
/// trait uses an opaque token pattern: `before_query` is called before
/// execution and returns a token (e.g., a file offset), which is passed to
/// `after_query` after execution to extract the stats.
///
/// # Thread Safety
///
/// Providers must be `Send + Sync` since they may be shared across connections.
pub trait QueryStatsProvider: Send + Sync {
    /// Called before a query is executed.
    ///
    /// Returns an opaque token that will be passed to [`after_query`](Self::after_query).
    /// For the log-based provider, this is the current file offset.
    fn before_query(&self, sql: &str) -> Box<dyn Any + Send>;

    /// Called after a query completes.
    ///
    /// Uses the token from [`before_query`](Self::before_query) to locate and
    /// extract the query statistics. Returns `None` if stats could not be found.
    fn after_query(&self, token: Box<dyn Any + Send>, sql: &str) -> Option<QueryStats>;
}

impl fmt::Debug for dyn QueryStatsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<QueryStatsProvider>")
    }
}

// =============================================================================
// LogFileStatsProvider
// =============================================================================

/// A [`QueryStatsProvider`] that extracts stats by parsing Hyper's JSON log file.
///
/// Hyper writes detailed `query-end` log entries to `hyperd.log` in JSON-per-line
/// format. This provider:
/// 1. Records the file offset before each query
/// 2. After the query, reads new log entries from that offset
/// 3. Finds the matching `query-end` entry by query text prefix
/// 4. Parses the JSON stats into a [`QueryStats`] struct
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::LogFileStatsProvider;
///
/// // From an explicit path
/// let provider = LogFileStatsProvider::new("/path/to/hyperd.log");
///
/// // From a HyperProcess (auto-detects log path)
/// // let provider = LogFileStatsProvider::from_process(&hyper);
/// ```
pub struct LogFileStatsProvider {
    log_path: PathBuf,
}

impl fmt::Debug for LogFileStatsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogFileStatsProvider")
            .field("log_path", &self.log_path)
            .finish()
    }
}

/// Opaque token storing the log file offset before a query.
struct LogFileToken {
    offset: u64,
}

impl LogFileStatsProvider {
    /// Creates a new provider that reads from the given log file path.
    ///
    /// The path should point to the `hyperd.log` file written by the Hyper server.
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        LogFileStatsProvider {
            log_path: log_path.into(),
        }
    }

    /// Creates a new provider by auto-detecting the log path from a [`HyperProcess`].
    ///
    /// The log file is expected at `<log_dir>/hyperd.log`. If the process has no
    /// log directory, `hyperd.log` in the current directory is used.
    ///
    /// [`HyperProcess`]: crate::HyperProcess
    #[must_use]
    pub fn from_process(process: &crate::HyperProcess) -> Self {
        let log_dir = process.log_dir().unwrap_or_else(|| Path::new("."));
        LogFileStatsProvider {
            log_path: log_dir.join("hyperd.log"),
        }
    }

    /// Returns the log file path this provider reads from.
    #[must_use]
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Gets the current file size (used as offset marker).
    fn current_offset(&self) -> u64 {
        std::fs::metadata(&self.log_path).map_or(0, |m| m.len())
    }

    /// Reads new log entries since `offset` and finds a matching `query-end` entry.
    ///
    /// Hyper's extended query protocol (used by `query_streaming`) produces two
    /// `query-end` entries per query:
    /// 1. A PREPARE entry with the original SQL text but `rows=0` and minimal stats.
    /// 2. An execution entry with the fully-qualified (schema-prefixed) SQL and
    ///    the real rows, memory, storage I/O, and timing stats.
    ///
    /// We prefer execution entries (non-PREPARE) over PREPARE entries, and use
    /// keyword-based matching to handle Hyper's query rewriting.
    fn find_query_end(&self, offset: u64, sql: &str) -> Option<QueryStats> {
        let file = File::open(&self.log_path).ok()?;
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(offset)).ok()?;

        // Normalize the SQL for matching against the truncated log entry
        let sql_normalized = normalize_for_matching(sql);
        // Extract significant keywords/identifiers for fuzzy matching
        let sql_tokens = extract_match_tokens(&sql_normalized);

        // Collect all query-end entries, then pick the best one
        let mut prepare_match: Option<QueryStats> = None;
        let mut execution_match: Option<QueryStats> = None;
        let mut last_entry: Option<QueryStats> = None;

        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(_) => break,
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Fast check before JSON parsing
            if !trimmed.contains("\"query-end\"") {
                continue;
            }

            let entry: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Verify this is a query-end entry
            if entry.get("k").and_then(|k| k.as_str()) != Some("query-end") {
                continue;
            }

            let Some(v) = entry.get("v") else { continue };

            let is_prepare = v
                .get("statement")
                .and_then(|s| s.as_str())
                .is_some_and(|s| s == "PREPARE");

            let is_prepare_flag = v
                .get("prepare-statement")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            let is_prepare = is_prepare || is_prepare_flag;

            // Check if this entry matches our query
            let matches = if let Some(query_trunc) = v.get("query-trunc").and_then(|q| q.as_str()) {
                let log_normalized = normalize_for_matching(query_trunc);
                // Direct prefix match (either direction)
                sql_normalized.starts_with(&log_normalized)
                    || log_normalized.starts_with(&sql_normalized)
                    // Token-based match: all significant tokens from our SQL appear in the log entry
                    || (!sql_tokens.is_empty()
                        && sql_tokens.iter().all(|t| log_normalized.contains(t)))
            } else {
                false
            };

            if matches {
                trace!(
                    target: "hyperdb_api",
                    query_trunc = v.get("query-trunc").and_then(|q| q.as_str()).unwrap_or(""),
                    is_prepare,
                    "query-stats-matched"
                );
                if is_prepare {
                    prepare_match = Some(parse_query_end(v));
                } else {
                    execution_match = Some(parse_query_end(v));
                }
            }

            // Always track the last entry as a fallback
            last_entry = Some(parse_query_end(v));
        }

        // Prefer: execution match > prepare match > last entry (for SET/ATTACH etc.)
        execution_match.or(prepare_match).or(last_entry)
    }
}

impl QueryStatsProvider for LogFileStatsProvider {
    fn before_query(&self, _sql: &str) -> Box<dyn Any + Send> {
        let offset = self.current_offset();
        trace!(
            target: "hyperdb_api",
            offset,
            path = %self.log_path.display(),
            "query-stats-before"
        );
        Box::new(LogFileToken { offset })
    }

    fn after_query(&self, token: Box<dyn Any + Send>, sql: &str) -> Option<QueryStats> {
        let token = token.downcast::<LogFileToken>().ok()?;

        // Small delay to allow the OS to flush Hyper's log write to disk.
        // Hyper logs query-end before sending the result, so by the time the
        // client has consumed all result data the entry should be written,
        // but OS-level I/O buffering may introduce a small delay.
        std::thread::sleep(std::time::Duration::from_millis(5));

        let stats = self.find_query_end(token.offset, sql);

        if stats.is_none() {
            debug!(
                target: "hyperdb_api",
                offset = token.offset,
                sql_prefix = log_prefix(sql),
                "query-stats-not-found"
            );
        }

        stats
    }
}

/// Returns at most the first 80 bytes of `sql` for log output, cut on a
/// character boundary so multi-byte text cannot cause a slicing panic.
fn log_prefix(sql: &str) -> &str {
    let mut end = sql.len().min(80);
    while !sql.is_char_boundary(end) {
        end -= 1;
    }
    &sql[..end]
}

// =============================================================================
// JSON Parsing Helpers
// =============================================================================

/// Extracts significant tokens (table names, column names, keywords) from
/// normalized SQL for fuzzy matching against Hyper's rewritten queries.
///
/// Hyper rewrites queries with schema prefixes (e.g., `employees` becomes
/// `"reading_data"."public"."employees"`), so we extract user-written
/// identifiers and keywords to match against the rewritten form.
fn extract_match_tokens(normalized_sql: &str) -> Vec<String> {
    // SQL keywords to skip — these appear in every query and aren't useful for matching
    const SKIP: &[&str] = &[
        "select",
        "from",
        "where",
        "and",
        "or",
        "not",
        "in",
        "is",
        "null",
        "as",
        "order",
        "by",
        "group",
        "having",
        "limit",
        "offset",
        "join",
        "on",
        "left",
        "right",
        "inner",
        "outer",
        "cross",
        "full",
        "union",
        "all",
        "distinct",
        "insert",
        "into",
        "values",
        "update",
        "set",
        "delete",
        "create",
        "drop",
        "alter",
        "table",
        "temporary",
        "temp",
        "if",
        "exists",
        "index",
        "with",
        "case",
        "when",
        "then",
        "else",
        "end",
        "between",
        "like",
        "cast",
        "asc",
        "desc",
        "true",
        "false",
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "text",
        "int",
        "integer",
        "bigint",
        "smallint",
        "double",
        "precision",
        "float",
        "varchar",
        "bool",
        "boolean",
        "date",
        "timestamp",
        "interval",
        "generate_series",
    ];

    normalized_sql
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 2)
        .map(str::to_lowercase)
        .filter(|t| !SKIP.contains(&t.as_str()))
        .collect()
}

/// Normalizes SQL text for matching against truncated log entries.
///
/// Lowercases and collapses whitespace so that `SELECT * FROM Test` matches
/// `select * from "db"."public"."test"`.
fn normalize_for_matching(sql: &str) -> String {
    let mut result = String::with_capacity(sql.len());
    let mut prev_was_space = false;
    for c in sql.chars() {
        if c.is_whitespace() {
            if !prev_was_space {
                result.push(' ');
                prev_was_space = true;
            }
        } else {
            // Lowercase for case-insensitive matching
            for lc in c.to_lowercase() {
                result.push(lc);
            }
            prev_was_space = false;
        }
    }
    result.trim().to_string()
}

/// Parses a `query-end` log entry's `v` (value) object into a `QueryStats`.
fn parse_query_end(v: &Value) -> QueryStats {
    // Pre-execution stats
    let pre_execution = v.get("pre-execution").map(|pre| PreExecutionStats {
        parsing_time_s: pre.get("parsing-time").and_then(serde_json::Value::as_f64),
        compilation_time_s: pre
            .get("compilation-time")
            .and_then(serde_json::Value::as_f64),
        elapsed_s: pre.get("elapsed").and_then(serde_json::Value::as_f64),
        peak_memory_mb: pre
            .get("peak-transaction-memory-mb")
            .and_then(serde_json::Value::as_f64),
    });

    // Execution stats
    let execution = v.get("execution").map(|exec| {
        let (cpu_time_s, thread_time_s, wait_time_s) = if let Some(threads) = exec.get("threads") {
            (
                threads.get("cpu-time").and_then(serde_json::Value::as_f64),
                threads
                    .get("thread-time")
                    .and_then(serde_json::Value::as_f64),
                threads.get("wait-time").and_then(serde_json::Value::as_f64),
            )
        } else {
            (None, None, None)
        };

        let (processed_rows_total, processed_rows_native) =
            if let Some(rows) = exec.get("processed-rows") {
                (
                    rows.get("total").and_then(serde_json::Value::as_u64),
                    rows.get("native").and_then(serde_json::Value::as_u64),
                )
            } else {
                (None, None)
            };

        let (storage_access_time_s, storage_access_count, storage_access_bytes) =
            if let Some(storage) = exec.get("storage") {
                (
                    storage
                        .get("access-time")
                        .and_then(serde_json::Value::as_f64),
                    storage
                        .get("access-count")
                        .and_then(serde_json::Value::as_u64),
                    storage
                        .get("access-bytes")
                        .and_then(serde_json::Value::as_u64),
                )
            } else {
                (None, None, None)
            };

        ExecutionStats {
            elapsed_s: exec.get("elapsed").and_then(serde_json::Value::as_f64),
            peak_memory_mb: exec
                .get("peak-transaction-memory-mb")
                .and_then(serde_json::Value::as_f64),
            cpu_time_s,
            thread_time_s,
            wait_time_s,
            processed_rows_total,
            processed_rows_native,
            storage_access_time_s,
            storage_access_count,
            storage_access_bytes,
        }
    });

    QueryStats {
        elapsed_s: v
            .get("elapsed")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
        commit_time_s: v.get("commit-time").and_then(serde_json::Value::as_f64),
        time_to_schedule_s: v
            .get("time-to-schedule")
            .and_then(serde_json::Value::as_f64),
        result_size_mb: v.get("result-size-mb").and_then(serde_json::Value::as_f64),
        peak_result_buffer_memory_mb: v
            .get("peak-result-buffer-memory-mb")
            .and_then(serde_json::Value::as_f64),
        plan_cache_status: v
            .get("plan-cache-status")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string),
        plan_cache_hit_count: v
            .get("plan-cache-hit-count")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        statement_type: v
            .get("statement")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string),
        rows: v.get("rows").and_then(serde_json::Value::as_u64),
        cols: v
            .get("cols")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        query_truncated: v
            .get("query-trunc")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string),
        pre_execution,
        execution,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_prefix_never_splits_a_multibyte_char() {
        // 79 ASCII bytes followed by a 3-byte char straddling the 80-byte cut.
        let sql = format!("{}€ tail", "a".repeat(79));
        let prefix = log_prefix(&sql);
        assert!(prefix.len() <= 80);
        assert_eq!(prefix, "a".repeat(79));
        assert_eq!(log_prefix("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn test_normalize_for_matching() {
        assert_eq!(
            normalize_for_matching("SELECT  *  FROM\n  test"),
            "select * from test"
        );
        assert_eq!(normalize_for_matching("  Hello   World  "), "hello world");
    }

    #[test]
    fn test_parse_query_end_full() {
        let json = r#"{
            "elapsed": 0.0299386,
            "commit-time": 1.666e-06,
            "time-to-schedule": 3.6208e-05,
            "result-size-mb": 0.00053978,
            "plan-cache-status": "cache miss",
            "plan-cache-hit-count": 0,
            "statement": "SELECT",
            "rows": 42,
            "cols": 3,
            "query-trunc": "SELECT * FROM test",
            "pre-execution": {
                "parsing-time": 1.75e-05,
                "compilation-time": 1.4542e-05,
                "elapsed": 2.45e-05,
                "peak-transaction-memory-mb": 0.5
            },
            "execution": {
                "elapsed": 0.0293959,
                "peak-transaction-memory-mb": 1.25,
                "threads": {
                    "thread-time": 0.0293959,
                    "cpu-time": 0.029353,
                    "wait-time": 0.0001
                },
                "processed-rows": {
                    "total": 1000,
                    "native": 1000
                },
                "storage": {
                    "access-time": 0.000529375,
                    "access-count": 11,
                    "access-bytes": 148979
                }
            }
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let stats = parse_query_end(&v);

        assert!((stats.elapsed_s - 0.0299386).abs() < 1e-10);
        assert_eq!(stats.plan_cache_status, Some("cache miss".to_string()));
        assert_eq!(stats.rows, Some(42));
        assert_eq!(stats.cols, Some(3));

        let pre = stats.pre_execution.unwrap();
        assert!((pre.parsing_time_s.unwrap() - 1.75e-05).abs() < 1e-15);
        assert!((pre.compilation_time_s.unwrap() - 1.4542e-05).abs() < 1e-15);
        assert!((pre.peak_memory_mb.unwrap() - 0.5).abs() < 1e-10);

        let exec = stats.execution.unwrap();
        assert!((exec.elapsed_s.unwrap() - 0.0293959).abs() < 1e-10);
        assert_eq!(exec.processed_rows_total, Some(1000));
        assert_eq!(exec.storage_access_count, Some(11));
        assert_eq!(exec.storage_access_bytes, Some(148979));
    }

    #[test]
    fn test_parse_query_end_minimal() {
        let json = r#"{"elapsed": 0.001}"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let stats = parse_query_end(&v);

        assert!((stats.elapsed_s - 0.001).abs() < 1e-10);
        assert!(stats.pre_execution.is_none());
        assert!(stats.execution.is_none());
        assert!(stats.plan_cache_status.is_none());
    }
}
