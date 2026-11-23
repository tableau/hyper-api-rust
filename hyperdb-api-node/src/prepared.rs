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
        let encoded = self.encode(params)?;
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
        let encoded = self.encode(params)?;
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
        let encoded = self.encode(params)?;
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
        let encoded = self.encode(params)?;
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

    /// Encodes JS values: coerced to the declared OIDs for `prepareTyped`
    /// statements, inferred from the JS value otherwise.
    fn encode(&self, params: Vec<serde_json::Value>) -> Result<Vec<ParamValue>> {
        match &self.explicit_oids {
            Some(oids) => coerce_params(oids, params).map_err(Error::from_reason),
            None => Ok(encode_json_params(params)),
        }
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
    // With explicit OIDs the statement is prepared once (eagerly in
    // `prepareTyped`) and never re-prepared: values are coerced to the
    // declared OIDs instead (see `coerce_params`).
    let needs_new = match (guard.as_ref(), explicit) {
        (Some(_), Some(_)) => false,
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
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Numeric(hyperdb_api::Numeric),
    String(String),
    /// Text-format value bound under an explicit declared OID (DATE, TIME,
    /// TIMESTAMP, ...): Hyper parses the text itself.
    TypedText(hyperdb_api::Oid, String),
    // Buffer params aren't reachable via serde_json::Value today — kept
    // so the ToSqlParam impl stays complete for future JS-side wiring.
    Bytes(Vec<u8>),
}

impl hyperdb_api::ToSqlParam for ParamValue {
    fn encode_param(&self) -> Option<Vec<u8>> {
        match self {
            ParamValue::Null => None,
            ParamValue::Bool(v) => v.encode_param(),
            ParamValue::I16(v) => v.encode_param(),
            ParamValue::I32(v) => v.encode_param(),
            ParamValue::F32(v) => v.encode_param(),
            ParamValue::Numeric(v) => v.encode_param(),
            ParamValue::TypedText(_, v) => v.encode_param(),
            ParamValue::I64(v) => v.encode_param(),
            ParamValue::F64(v) => v.encode_param(),
            ParamValue::String(v) => v.encode_param(),
            ParamValue::Bytes(v) => v.as_slice().encode_param(),
        }
    }

    fn param_format(&self) -> hyperdb_api::ParamFormat {
        match self {
            ParamValue::TypedText(..) => hyperdb_api::ParamFormat::Text,
            ParamValue::Numeric(v) => v.param_format(),
            _ => hyperdb_api::ParamFormat::Binary,
        }
    }

    fn sql_oid(&self) -> hyperdb_api::Oid {
        match self {
            // OID 0 on a NULL parameter defers the type to inference.
            // Hyper accepts this only when the surrounding SQL forces a
            // unique type (e.g. `WHERE col = $1` with a known col type).
            ParamValue::Null => hyperdb_api::Oid::new(0),
            ParamValue::Bool(v) => v.sql_oid(),
            ParamValue::I16(v) => v.sql_oid(),
            ParamValue::I32(v) => v.sql_oid(),
            ParamValue::F32(v) => v.sql_oid(),
            ParamValue::Numeric(v) => v.sql_oid(),
            ParamValue::TypedText(oid, _) => *oid,
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
            ParamValue::I16(v) => v.to_sql_literal(),
            ParamValue::I32(v) => v.to_sql_literal(),
            ParamValue::F32(v) => v.to_sql_literal(),
            ParamValue::Numeric(v) => v.to_sql_literal(),
            ParamValue::TypedText(_, v) => v.to_sql_literal(),
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

/// Largest magnitude below which every integer is exactly representable
/// as an `f64` (2^53).
const MAX_EXACT_F64_INT: i64 = 1 << 53;

/// Reads a JSON number as an exact `i64`: an integer, or a float with no
/// fractional part that lies within `i64` range.
fn exact_i64(n: &serde_json::Number) -> Option<i64> {
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    // `-2^63 <= f < 2^63` and integral, so the conversion is exact.
    if f.fract() == 0.0 && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f)
    {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "guarded above: `f` is integral and within the i64 range"
        )]
        let i = f as i64;
        Some(i)
    } else {
        None
    }
}

/// Coerces one JS value to the width and format required by the declared
/// parameter `oid`. Errors (never saturates or wraps) when the value does
/// not fit or has the wrong JS type.
fn coerce_param(
    oid: hyperdb_api::Oid,
    idx: usize,
    val: serde_json::Value,
) -> std::result::Result<ParamValue, String> {
    use hyperdb_api::oids;
    use serde_json::Value;

    let n = idx + 1;
    let bad = |what: &str, val: &Value| {
        format!(
            "parameter ${n}: cannot bind JS value {val} as {what} (declared OID {})",
            oid.0
        )
    };
    if val.is_null() {
        return Ok(ParamValue::Null);
    }
    match oid {
        o if o == oids::SMALL_INT || o == oids::INT || o == oids::BIG_INT => {
            let name = if o == oids::SMALL_INT {
                "SMALLINT"
            } else if o == oids::INT {
                "INTEGER"
            } else {
                "BIGINT"
            };
            let i = match &val {
                Value::Number(num) => exact_i64(num),
                _ => None,
            }
            .ok_or_else(|| bad(name, &val))?;
            if o == oids::SMALL_INT {
                i16::try_from(i)
                    .map(ParamValue::I16)
                    .map_err(|_| bad(name, &val))
            } else if o == oids::INT {
                i32::try_from(i)
                    .map(ParamValue::I32)
                    .map_err(|_| bad(name, &val))
            } else {
                Ok(ParamValue::I64(i))
            }
        }
        o if o == oids::DOUBLE || o == oids::FLOAT => {
            let name = if o == oids::DOUBLE {
                "DOUBLE PRECISION"
            } else {
                "REAL"
            };
            let f = match &val {
                Value::Number(num) => match num.as_i64() {
                    // Integers must be exactly representable as f64.
                    Some(i) if i.abs() > MAX_EXACT_F64_INT => None,
                    Some(i) => {
                        // |i| <= 2^53 here, so the conversion is exact.
                        #[expect(
                            clippy::cast_precision_loss,
                            reason = "guarded above: |i| <= 2^53 is exact in f64"
                        )]
                        let exact = i as f64;
                        Some(exact)
                    }
                    None => num.as_f64().filter(|f| f.is_finite()),
                },
                _ => None,
            }
            .ok_or_else(|| bad(name, &val))?;
            if o == oids::DOUBLE {
                Ok(ParamValue::F64(f))
            } else if f.abs() > f64::from(f32::MAX) {
                Err(bad(name, &val))
            } else {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "float narrowing: range-checked above; rounding to f32 precision is the declared REAL semantics"
                )]
                let narrowed = f as f32;
                Ok(ParamValue::F32(narrowed))
            }
        }
        o if o == oids::BOOL => match val {
            Value::Bool(b) => Ok(ParamValue::Bool(b)),
            _ => Err(bad("BOOLEAN", &val)),
        },
        o if o == oids::TEXT || o == oids::VARCHAR || o == oids::CHAR => match val {
            Value::String(s) => Ok(ParamValue::String(s)),
            _ => Err(bad("TEXT", &val)),
        },
        o if o == oids::NUMERIC => {
            // A declared NUMERIC OID carries no scale (Hyper resolves it to
            // NUMERIC(1,0)), so only whole numbers can bind; scaled values
            // would be rejected by the server (22003).
            let i = match &val {
                Value::Number(num) => exact_i64(num),
                _ => None,
            }
            .ok_or_else(|| {
                format!(
                    "{}; only whole numbers can bind to a declared NUMERIC (use CAST($n AS NUMERIC(p,s)) with a string for scaled values)",
                    bad("NUMERIC", &val)
                )
            })?;
            Ok(ParamValue::Numeric(hyperdb_api::Numeric::from_i64(i)))
        }
        o if o == oids::DATE
            || o == oids::TIME
            || o == oids::TIMESTAMP
            || o == oids::TIMESTAMP_TZ =>
        {
            match val {
                Value::String(s) => Ok(ParamValue::TypedText(o, s)),
                _ => Err(bad("a date/time string", &val)),
            }
        }
        // Other OIDs (BYTEA, JSON, INTERVAL, 0, ...) keep the value-inferred
        // encoding; the server validates it against the declared type.
        _ => Ok(json_value_to_param(val)),
    }
}

/// Encodes JS-supplied parameters for a statement prepared with explicit
/// OIDs, coercing each value to its declared type.
fn coerce_params(
    oids: &[hyperdb_api::Oid],
    params: Vec<serde_json::Value>,
) -> std::result::Result<Vec<ParamValue>, String> {
    if oids.len() != params.len() {
        return Err(format!(
            "statement was prepared with {} typed parameter(s) but {} value(s) were supplied",
            oids.len(),
            params.len()
        ));
    }
    oids.iter()
        .zip(params)
        .enumerate()
        .map(|(i, (oid, val))| coerce_param(*oid, i, val))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperdb_api::{Oid, ToSqlParam};
    use serde_json::json;

    fn oid(n: u32) -> Oid {
        Oid::new(n)
    }

    fn one(o: u32, v: serde_json::Value) -> std::result::Result<ParamValue, String> {
        coerce_params(&[oid(o)], vec![v]).map(|mut p| p.remove(0))
    }

    fn bytes(o: u32, v: serde_json::Value) -> Vec<u8> {
        one(o, v)
            .expect("coerces")
            .encode_param()
            .expect("non-null")
    }

    #[test]
    fn float8_given_integer_is_eight_byte_double() {
        let p = one(701, json!(5)).expect("coerces");
        assert_eq!(p.sql_oid(), oid(701));
        assert_eq!(p.encode_param().unwrap(), 5.0_f64.to_be_bytes().to_vec());
    }

    #[test]
    fn float4_given_float_is_four_byte_real() {
        assert_eq!(bytes(700, json!(1.5)), 1.5_f32.to_be_bytes().to_vec());
        assert_eq!(bytes(700, json!(2)), 2.0_f32.to_be_bytes().to_vec());
    }

    #[test]
    fn int2_int4_int8_use_declared_width() {
        assert_eq!(bytes(21, json!(7)), 7_i16.to_be_bytes().to_vec());
        assert_eq!(bytes(23, json!(7)), 7_i32.to_be_bytes().to_vec());
        assert_eq!(bytes(20, json!(7)), 7_i64.to_be_bytes().to_vec());
        assert_eq!(bytes(20, json!(7.0)), 7_i64.to_be_bytes().to_vec());
    }

    #[test]
    fn integer_range_and_fraction_are_rejected() {
        assert!(one(21, json!(40_000)).is_err());
        assert!(one(23, json!(5_000_000_000_i64)).is_err());
        assert!(one(23, json!(1.5)).is_err());
        assert!(one(20, json!("7")).is_err());
        assert!(one(701, json!("1.5")).is_err());
        assert!(one(700, json!(1.0e300)).is_err());
        assert!(one(16, json!(1)).is_err());
    }

    #[test]
    fn numeric_whole_is_binary_numeric() {
        let p = one(1700, json!(12)).expect("coerces");
        assert_eq!(p.sql_oid(), oid(1700));
        assert_eq!(
            p.encode_param().unwrap(),
            hyperdb_api::Numeric::from_i64(12).encode_param().unwrap()
        );
        assert!(one(1700, json!(1.5)).is_err());
    }

    #[test]
    fn bool_text_and_temporal_strings() {
        assert_eq!(bytes(16, json!(true)), vec![1]);
        assert_eq!(bytes(25, json!("hi")), b"hi".to_vec());
        let d = one(1082, json!("2024-03-05")).expect("coerces");
        assert_eq!(d.sql_oid(), oid(1082));
        assert_eq!(d.param_format(), hyperdb_api::ParamFormat::Text);
        assert_eq!(d.encode_param().unwrap(), b"2024-03-05".to_vec());
        assert!(one(1082, json!(5)).is_err());
    }

    #[test]
    fn null_and_arity() {
        assert!(matches!(one(23, json!(null)), Ok(ParamValue::Null)));
        assert!(coerce_params(&[oid(23)], vec![]).is_err());
    }
}
