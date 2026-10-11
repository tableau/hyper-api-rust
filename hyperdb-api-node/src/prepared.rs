// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::sync::Arc;
use tokio::sync::Mutex;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::connection::Connection;
use crate::result::{RowData, extract_row};

// =============================================================================
// PreparedStatement
// =============================================================================

/// A server-side prepared statement bound to a `Connection`.
///
/// Prepared statements are the supported way to run parameterized
/// queries: parameter values are sent over the wire in native binary
/// format — no per-value string escaping, no SQL-injection surface.
///
/// ## Type inference
///
/// `Connection.prepare(sql)` defers server-side preparation until the
/// first call to `query`/`execute` because Hyper's Parse message
/// requires the exact set of parameter OIDs up front. The first call
/// prepares using the OIDs inferred from the JS parameter values, then
/// caches the prepared statement for subsequent calls.
///
/// If you later call `query` with *different* types for the same
/// placeholders (e.g. `[42]` then `[5_000_000_000n]`, INT then BIGINT; a `bigint` that fits `i32`, such as `42n`, binds as INT like `42`), the statement re-prepares
/// transparently. For predictable, cache-hot behavior pass explicit
/// OIDs via `Connection.prepareTyped(sql, oids)`.
///
/// @example
/// ```js
/// const stmt = await conn.prepare('SELECT * FROM users WHERE id = $1');
/// const rows = await stmt.query([42]);
/// const count = await stmt.execute([42]);
/// await stmt.close();
/// ```
#[napi]
#[derive(Debug)]
pub struct PreparedStatement {
    sql: String,
    /// Fixed OIDs from `prepareTyped`. When `None`, OIDs are inferred
    /// per-call from the parameter values.
    explicit_oids: Option<Vec<hyperdb_api::Oid>>,
    /// Connection Arc used to (re)prepare on demand.
    conn: Arc<hyperdb_api::AsyncConnection>,
    /// Lazily prepared handle, rebuilt when the inferred OIDs change.
    cached: Arc<Mutex<Option<CachedPrepared>>>,
    /// Declared param count — `explicit_oids.len()` when typed,
    /// otherwise `count_placeholders(sql)`.
    param_count: u32,
    /// True until `close()` is called.
    alive: Arc<std::sync::atomic::AtomicBool>,
}

struct CachedPrepared {
    oids: Vec<hyperdb_api::Oid>,
    stmt: hyperdb_api::AsyncPreparedStatementOwned,
}

impl std::fmt::Debug for CachedPrepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedPrepared")
            .field("oid_count", &self.oids.len())
            .finish_non_exhaustive()
    }
}

fn already_closed() -> Error {
    Error::from_reason("PreparedStatement is closed")
}

fn count_placeholders(sql: &str) -> u32 {
    let bytes = sql.as_bytes();
    let mut max: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            let mut n: u32 = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                n = n * 10 + u32::from(bytes[j] - b'0');
                j += 1;
            }
            if j > i + 1 && n > max {
                max = n;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    max
}

#[napi]
impl PreparedStatement {
    /// Number of parameters this statement expects.
    #[napi(getter)]
    pub fn param_count(&self) -> u32 {
        self.param_count
    }

    /// Original SQL text.
    #[napi(getter)]
    pub fn sql(&self) -> String {
        self.sql.clone()
    }

    /// Executes the statement and returns every row.
    #[napi(ts_args_type = "params: Array<number | bigint | string | boolean | null | Buffer>")]
    pub async fn query(&self, params: Vec<serde_json::Value>) -> Result<Vec<RowData>> {
        self.ensure_alive()?;
        let encoded = encode_json_params(params);
        let conn = Arc::clone(&self.conn);
        let cached = Arc::clone(&self.cached);
        let explicit = self.explicit_oids.clone();
        let sql = self.sql.clone();
        run_query(conn, cached, explicit, sql, encoded).await
    }

    /// Executes the statement as a command (INSERT/UPDATE/DELETE) and
    /// returns the number of affected rows.
    #[napi(ts_args_type = "params: Array<number | bigint | string | boolean | null | Buffer>")]
    pub async fn execute(&self, params: Vec<serde_json::Value>) -> Result<i64> {
        self.ensure_alive()?;
        let encoded = encode_json_params(params);
        let conn = Arc::clone(&self.conn);
        let cached = Arc::clone(&self.cached);
        let explicit = self.explicit_oids.clone();
        let sql = self.sql.clone();
        run_execute(conn, cached, explicit, sql, encoded).await
    }

    /// Fetches exactly one row; errors if the result is empty.
    #[napi(ts_args_type = "params: Array<number | bigint | string | boolean | null | Buffer>")]
    pub async fn fetch_one(&self, params: Vec<serde_json::Value>) -> Result<RowData> {
        self.ensure_alive()?;
        let encoded = encode_json_params(params);
        let conn = Arc::clone(&self.conn);
        let cached = Arc::clone(&self.cached);
        let explicit = self.explicit_oids.clone();
        let sql = self.sql.clone();
        run_fetch_one(conn, cached, explicit, sql, encoded).await
    }

    /// Fetches at most one row; returns `null` if the result is empty.
    #[napi(ts_args_type = "params: Array<number | bigint | string | boolean | null | Buffer>")]
    pub async fn fetch_optional(&self, params: Vec<serde_json::Value>) -> Result<Option<RowData>> {
        self.ensure_alive()?;
        let encoded = encode_json_params(params);
        let conn = Arc::clone(&self.conn);
        let cached = Arc::clone(&self.cached);
        let explicit = self.explicit_oids.clone();
        let sql = self.sql.clone();
        run_fetch_optional(conn, cached, explicit, sql, encoded).await
    }

    /// Returns the result-set schema (column metadata) for this prepared
    /// statement. Returns `None` if the statement has not been prepared on
    /// the server yet (no `query` / `execute` call has run).
    #[napi]
    pub async fn get_schema(&self) -> Option<Vec<crate::result::ResultColumnInfo>> {
        let guard = self.cached.lock().await;
        let cached = guard.as_ref()?;
        let schema = cached.stmt.schema();
        Some(
            schema
                .columns()
                .iter()
                .map(|col| crate::result::ResultColumnInfo {
                    name: col.name().to_string(),
                    type_name: col.sql_type().to_string(),
                    index: u32::try_from(col.index()).unwrap_or(u32::MAX),
                })
                .collect(),
        )
    }

    /// Explicitly releases the statement on the server.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.alive.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut guard = self.cached.lock().await;
        let _ = guard.take();
        Ok(())
    }

    fn ensure_alive(&self) -> Result<()> {
        if self.alive.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(())
        } else {
            Err(already_closed())
        }
    }
}

// =============================================================================
// Prepare entry points on Connection
// =============================================================================

#[napi]
impl Connection {
    /// Prepares a SQL statement for parameterized execution.
    ///
    /// Parameter types are inferred from the JS values passed on the
    /// first call; if those types change between calls the statement
    /// re-prepares transparently. For explicit OID control, use
    /// `prepareTyped`.
    #[napi]
    pub fn prepare(&self, sql: String) -> PreparedStatement {
        let param_count = count_placeholders(&sql);
        PreparedStatement {
            sql,
            explicit_oids: None,
            conn: self.inner_arc(),
            cached: Arc::new(Mutex::new(None)),
            param_count,
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// Prepares a SQL statement with explicit parameter type OIDs.
    ///
    /// Use this when the inferred types are ambiguous or when you want
    /// to pin the server-side plan cache entry.
    #[napi]
    pub async fn prepare_typed(
        &self,
        sql: String,
        param_oids: Vec<u32>,
    ) -> Result<PreparedStatement> {
        let oids: Vec<hyperdb_api::Oid> =
            param_oids.into_iter().map(hyperdb_api::Oid::new).collect();
        // Eagerly prepare so prepareTyped surfaces syntax errors up front.
        let stmt = self
            .inner_arc_ref()
            .prepare_typed_arc(&sql, &oids)
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?;
        // Prepared-statement parameter counts are structurally bounded by
        // Hyper's wire protocol (well below u32::MAX); saturating is a safe
        // diagnostic.
        let param_count = u32::try_from(stmt.param_count()).unwrap_or(u32::MAX);
        let cached = CachedPrepared {
            oids: oids.clone(),
            stmt,
        };
        Ok(PreparedStatement {
            sql,
            explicit_oids: Some(oids),
            conn: self.inner_arc(),
            cached: Arc::new(Mutex::new(Some(cached))),
            param_count,
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }
}

// =============================================================================
// Prepare + execute helper — lazy-prepare or re-prepare on OID mismatch
// =============================================================================

#[expect(
    clippy::ref_option,
    reason = "matches callers that already hold `&Option<T>`; avoiding a `.as_ref()` dance at every call site"
)]
/// Locks the cache and returns a fresh or cached prepared statement
/// compatible with `wanted_oids`. If the cache holds a statement with a
/// different OID signature, it is dropped and a new one is prepared.
async fn ensure_cached(
    conn: &Arc<hyperdb_api::AsyncConnection>,
    cache: &Arc<Mutex<Option<CachedPrepared>>>,
    explicit: &Option<Vec<hyperdb_api::Oid>>,
    sql: &str,
    wanted_oids: Vec<hyperdb_api::Oid>,
) -> Result<tokio::sync::OwnedMutexGuard<Option<CachedPrepared>>> {
    let mut guard = Arc::clone(cache).lock_owned().await;
    let needs_new = match (guard.as_ref(), explicit) {
        (Some(c), Some(_)) => c.oids != wanted_oids && explicit.is_none(),
        (Some(c), None) => c.oids != wanted_oids,
        (None, _) => true,
    };
    if needs_new {
        let oids = match explicit {
            Some(e) => e.clone(),
            None => wanted_oids,
        };
        let stmt = conn
            .prepare_typed_arc(sql, &oids)
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?;
        *guard = Some(CachedPrepared { oids, stmt });
    }
    Ok(guard)
}

fn oids_from(params: &[ParamValue]) -> Vec<hyperdb_api::Oid> {
    params
        .iter()
        .map(hyperdb_api::ToSqlParam::sql_oid)
        .collect()
}

// =============================================================================
// Execution paths
// =============================================================================

async fn run_query(
    conn: Arc<hyperdb_api::AsyncConnection>,
    cache: Arc<Mutex<Option<CachedPrepared>>>,
    explicit: Option<Vec<hyperdb_api::Oid>>,
    sql: String,
    params: Vec<ParamValue>,
) -> Result<Vec<RowData>> {
    let oids = oids_from(&params);
    let guard = ensure_cached(&conn, &cache, &explicit, &sql, oids).await?;
    let cached = guard.as_ref().expect("ensure_cached populates");
    let schema: hyperdb_api::ResultSchema = cached.stmt.schema().clone();
    let refs: Vec<&dyn hyperdb_api::ToSqlParam> = params
        .iter()
        .map(|p| p as &dyn hyperdb_api::ToSqlParam)
        .collect();
    let rows = cached
        .stmt
        .fetch_all(&refs)
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?;
    Ok(rows
        .into_iter()
        .map(|row| RowData {
            values: extract_row(&row, &schema),
        })
        .collect())
}

async fn run_execute(
    conn: Arc<hyperdb_api::AsyncConnection>,
    cache: Arc<Mutex<Option<CachedPrepared>>>,
    explicit: Option<Vec<hyperdb_api::Oid>>,
    sql: String,
    params: Vec<ParamValue>,
) -> Result<i64> {
    let oids = oids_from(&params);
    let guard = ensure_cached(&conn, &cache, &explicit, &sql, oids).await?;
    let cached = guard.as_ref().expect("ensure_cached populates");
    let refs: Vec<&dyn hyperdb_api::ToSqlParam> = params
        .iter()
        .map(|p| p as &dyn hyperdb_api::ToSqlParam)
        .collect();
    cached
        .stmt
        .execute(&refs)
        .await
        .map_err(|e| Error::from_reason(e.to_string()))
        .and_then(|n| {
            i64::try_from(n).map_err(|_| Error::from_reason("row count exceeds i64::MAX"))
        })
}

async fn run_fetch_one(
    conn: Arc<hyperdb_api::AsyncConnection>,
    cache: Arc<Mutex<Option<CachedPrepared>>>,
    explicit: Option<Vec<hyperdb_api::Oid>>,
    sql: String,
    params: Vec<ParamValue>,
) -> Result<RowData> {
    let oids = oids_from(&params);
    let guard = ensure_cached(&conn, &cache, &explicit, &sql, oids).await?;
    let cached = guard.as_ref().expect("ensure_cached populates");
    let schema: hyperdb_api::ResultSchema = cached.stmt.schema().clone();
    let refs: Vec<&dyn hyperdb_api::ToSqlParam> = params
        .iter()
        .map(|p| p as &dyn hyperdb_api::ToSqlParam)
        .collect();
    let row = cached
        .stmt
        .fetch_one(&refs)
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?;
    Ok(RowData {
        values: extract_row(&row, &schema),
    })
}

async fn run_fetch_optional(
    conn: Arc<hyperdb_api::AsyncConnection>,
    cache: Arc<Mutex<Option<CachedPrepared>>>,
    explicit: Option<Vec<hyperdb_api::Oid>>,
    sql: String,
    params: Vec<ParamValue>,
) -> Result<Option<RowData>> {
    let oids = oids_from(&params);
    let guard = ensure_cached(&conn, &cache, &explicit, &sql, oids).await?;
    let cached = guard.as_ref().expect("ensure_cached populates");
    let schema: hyperdb_api::ResultSchema = cached.stmt.schema().clone();
    let refs: Vec<&dyn hyperdb_api::ToSqlParam> = params
        .iter()
        .map(|p| p as &dyn hyperdb_api::ToSqlParam)
        .collect();
    let opt = cached
        .stmt
        .fetch_optional(&refs)
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?;
    Ok(opt.map(|row| RowData {
        values: extract_row(&row, &schema),
    }))
}

// =============================================================================
// JS → ToSqlParam conversion
// =============================================================================

/// Owned value type that implements `ToSqlParam`. We need concrete
/// storage so the `&dyn ToSqlParam` refs stay valid across the await.
#[derive(Debug, Clone)]
#[expect(
    dead_code,
    reason = "Bytes variant kept so the ToSqlParam impl stays complete for future JS-side wiring"
)]
enum ParamValue {
    Null,
    Bool(bool),
    I32(i32),
    I64(i64),
    F64(f64),
    String(String),
    // Buffer params aren't reachable via serde_json::Value today — kept
    // so the ToSqlParam impl stays complete for future JS-side wiring.
    Bytes(Vec<u8>),
}

impl hyperdb_api::ToSqlParam for ParamValue {
    fn encode_param(&self) -> Option<Vec<u8>> {
        match self {
            ParamValue::Null => None,
            ParamValue::Bool(v) => v.encode_param(),
            ParamValue::I32(v) => v.encode_param(),
            ParamValue::I64(v) => v.encode_param(),
            ParamValue::F64(v) => v.encode_param(),
            ParamValue::String(v) => v.encode_param(),
            ParamValue::Bytes(v) => v.as_slice().encode_param(),
        }
    }

    fn sql_oid(&self) -> hyperdb_api::Oid {
        match self {
            // OID 0 on a NULL parameter defers the type to inference.
            // Hyper accepts this only when the surrounding SQL forces a
            // unique type (e.g. `WHERE col = $1` with a known col type).
            ParamValue::Null => hyperdb_api::Oid::new(0),
            ParamValue::Bool(v) => v.sql_oid(),
            ParamValue::I32(v) => v.sql_oid(),
            ParamValue::I64(v) => v.sql_oid(),
            ParamValue::F64(v) => v.sql_oid(),
            ParamValue::String(v) => v.sql_oid(),
            ParamValue::Bytes(v) => v.as_slice().sql_oid(),
        }
    }

    fn to_sql_literal(&self) -> String {
        match self {
            ParamValue::Null => "NULL".to_string(),
            ParamValue::Bool(v) => v.to_sql_literal(),
            ParamValue::I32(v) => v.to_sql_literal(),
            ParamValue::I64(v) => v.to_sql_literal(),
            ParamValue::F64(v) => v.to_sql_literal(),
            ParamValue::String(v) => v.to_sql_literal(),
            ParamValue::Bytes(v) => v.as_slice().to_sql_literal(),
        }
    }
}

fn encode_json_params(params: Vec<serde_json::Value>) -> Vec<ParamValue> {
    params.into_iter().map(json_value_to_param).collect()
}

fn json_value_to_param(val: serde_json::Value) -> ParamValue {
    use serde_json::Value;
    match val {
        Value::Null => ParamValue::Null,
        Value::Bool(b) => ParamValue::Bool(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&i) {
                    // Guard above restricts `i` to `[i32::MIN, i32::MAX]`;
                    // the narrowing is a reinterpret of an already-bounded
                    // integer.
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "guarded above: `i` is in `[i32::MIN, i32::MAX]`"
                    )]
                    let narrowed = i as i32;
                    ParamValue::I32(narrowed)
                } else {
                    ParamValue::I64(i)
                }
            } else if let Some(f) = n.as_f64() {
                ParamValue::F64(f)
            } else {
                ParamValue::Null
            }
        }
        Value::String(s) => ParamValue::String(s),
        // Arrays and objects don't have a natural scalar mapping — pass
        // as the JSON stringification for JSON/JSONB columns.
        Value::Array(_) | Value::Object(_) => ParamValue::String(val.to_string()),
    }
}
