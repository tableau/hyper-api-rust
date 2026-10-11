// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Prepared statement support using extended query protocol.
//!
//! # Parameter Encoding
//!
//! Use the [`params!`](crate::params) macro for ergonomic parameter encoding:
//!
//! ```no_run
//! # use hyperdb_api_core::{params, client::{Client, Config}};
//! # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
//! let stmt = client.prepare("SELECT * FROM users WHERE id = $1 AND name = $2")?;
//! let rows = client.execute(&stmt, params![42_i32, "Alice"])?;
//! # Ok(())
//! # }
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::protocol::message::{backend::Message, frontend};
use crate::types::Oid;
use tracing::{trace, warn};

use super::connection::RawConnection;
use super::error::{Error, Result};
use super::row::Row;
use super::statement::{Column, ParamFormat, bind_format_codes};
use super::sync_stream::SyncStream;
// =============================================================================
// SqlParam trait - binary parameter encoding
// =============================================================================

/// Trait for types that can be encoded as SQL prepared statement parameters.
///
/// This trait enables the [`params!`](crate::params) macro to automatically encode values.
pub trait SqlParam {
    /// Encodes the value as binary bytes.
    fn encode(&self) -> Vec<u8>;
}

impl SqlParam for i16 {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl SqlParam for i32 {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl SqlParam for i64 {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl SqlParam for f32 {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl SqlParam for f64 {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}

impl SqlParam for bool {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        vec![u8::from(*self)]
    }
}

impl SqlParam for &str {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}

impl SqlParam for String {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}

impl SqlParam for &String {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}

impl SqlParam for Vec<u8> {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.clone()
    }
}

impl SqlParam for &[u8] {
    #[inline]
    fn encode(&self) -> Vec<u8> {
        self.to_vec()
    }
}

/// Macro for building prepared statement parameters with automatic encoding.
///
/// # Examples
///
/// ```no_run
/// # use hyperdb_api_core::{params, client::{Client, Config, SqlParam}};
/// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
/// let stmt = client.prepare("SELECT * FROM t WHERE id = $1 AND name = $2")?;
///
/// // Pass typed values directly
/// let rows = client.execute(&stmt, params![42_i32, "Alice"])?;
///
/// // For NULL values, use None explicitly
/// let rows = client.execute(&stmt, &[Some(42_i32.encode()), None])?;
/// # Ok(())
/// # }
/// ```
#[macro_export]
macro_rules! params {
    () => {
        &[] as &[Option<Vec<u8>>]
    };
    ($($val:expr),+ $(,)?) => {{
        use $crate::client::prepare::SqlParam;
        vec![$(Some($val.encode())),+]
    }};
}

/// Counter for generating unique statement names.
static STATEMENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generates a unique statement name.
fn generate_statement_name() -> String {
    let id = STATEMENT_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("__hyper_stmt_{id}")
}

/// A prepared statement.
///
/// Prepared statements allow you to execute the same query multiple times
/// with different parameters efficiently. The statement is prepared once on
/// the server and can be executed many times with different parameter values.
///
/// Usually obtained through [`OwnedPreparedStatement::statement`]; [`Client::prepare`](crate::client::Client::prepare) returns the owned, self-closing wrapper.
///
/// # Example
///
/// ```no_run
/// # use hyperdb_api_core::{params, client::{Client, Config}};
/// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
/// let stmt = client.prepare("SELECT * FROM users WHERE id = $1")?;
/// let rows1 = client.execute(&stmt, params![42_i32])?;
/// let rows2 = client.execute(&stmt, params![100_i32])?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct PreparedStatement {
    /// Statement name on the server (used for Bind/Execute messages).
    name: String,
    /// Original SQL query string.
    query: String,
    /// Parameter type OIDs from the server's `ParameterDescription`, one per placeholder.
    param_types: Vec<Oid>,
    /// Result column descriptions from the `RowDescription` returned at prepare time (empty for statements without results).
    columns: Vec<Column>,
}

/// A prepared statement that automatically closes itself when dropped.
///
/// This is the recommended way to use prepared statements. It holds a weak
/// reference to the connection and automatically closes the statement when dropped.
///
/// # Example
///
/// ```no_run
/// # use hyperdb_api_core::{params, client::{Client, Config}};
/// # fn example(client: &Client) -> hyperdb_api_core::client::Result<()> {
/// // Statement automatically closes when it goes out of scope
/// {
///     let stmt = client.prepare("SELECT * FROM users WHERE id = $1")?;
///     let rows = client.execute(&stmt, params![42_i32])?;
/// } // Statement is automatically closed here
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct OwnedPreparedStatement {
    /// The underlying prepared statement.
    statement: PreparedStatement,
    /// Weak reference to the connection for cleanup.
    connection: Weak<Mutex<RawConnection<SyncStream>>>,
}

impl OwnedPreparedStatement {
    /// Creates a new owned prepared statement.
    pub(crate) fn new(
        statement: PreparedStatement,
        connection: &Arc<Mutex<RawConnection<SyncStream>>>,
    ) -> Self {
        OwnedPreparedStatement {
            statement,
            connection: Arc::downgrade(connection),
        }
    }

    /// Returns the statement name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.statement.name()
    }

    /// Returns the original query.
    #[must_use]
    pub fn query(&self) -> &str {
        self.statement.query()
    }

    /// Returns the parameter type OIDs the server reported for this statement.
    #[must_use]
    pub fn param_types(&self) -> &[Oid] {
        self.statement.param_types()
    }

    /// Returns the number of parameters.
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.statement.param_count()
    }

    /// Returns the result column descriptions.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        self.statement.columns()
    }

    /// Returns the number of result columns.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.statement.column_count()
    }

    /// Returns a reference to the underlying `PreparedStatement`.
    #[must_use]
    pub fn statement(&self) -> &PreparedStatement {
        &self.statement
    }

    /// Explicitly closes the statement, returning any error.
    ///
    /// This is called automatically when the `OwnedPreparedStatement` is
    /// dropped, but errors are silently ignored in that case. Use this
    /// method if you need to handle close errors.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`close_statement`] — connection
    /// mutex poisoning, server-side error during `Close`/`Sync`, or
    /// wire I/O failure. Returns `Ok(())` without contacting the server
    /// when the connection has already been dropped.
    pub fn close(self) -> Result<()> {
        if let Some(conn) = self.connection.upgrade() {
            close_statement(&conn, &self.statement)?;
        }
        // Don't run Drop since we've already closed
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for OwnedPreparedStatement {
    fn drop(&mut self) {
        // Best-effort cleanup - log errors but don't panic during drop
        if let Some(conn) = self.connection.upgrade()
            && let Err(e) = close_statement_internal(&conn, &self.statement)
        {
            warn!(
                target: "hyperdb_api",
                statement_name = %self.statement.name,
                error = %e,
                "failed-to-close-prepared-statement-during-drop"
            );
        }
        // If the connection is already dropped, we can't close the statement
        // but that's okay - the server will clean it up when the connection closes
    }
}

impl PreparedStatement {
    /// Returns the statement name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the original query.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Returns the parameter type OIDs the server reported for this statement.
    ///
    /// Hyper echoes the OIDs passed to `prepare_typed`, so an unspecified (`0`) OID reads back as `0`.
    #[must_use]
    pub fn param_types(&self) -> &[Oid] {
        &self.param_types
    }

    /// Returns the number of parameters.
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.param_types.len()
    }

    /// Returns the result column descriptions.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns the number of result columns.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }
}

/// Prepares a statement using the extended query protocol.
///
/// # Errors
///
/// - Returns [`Error`] (connection) if the connection mutex is poisoned.
/// - Returns [`Error::Query`] if the server rejects the `Parse` request
///   (SQL syntax error, unknown parameter OIDs, etc.).
/// - Returns [`Error`] (I/O) / [`Error`] (closed) on wire-protocol I/O
///   failure.
pub fn prepare(
    connection: &Arc<Mutex<RawConnection<SyncStream>>>,
    query: &str,
    param_types: &[Oid],
) -> Result<PreparedStatement> {
    let name = generate_statement_name();
    let mut conn = connection
        .lock()
        .map_err(|_| Error::connection("connection mutex poisoned"))?;

    // Send Parse message
    frontend::parse(&name, query, param_types, conn.write_buf())?;

    // Send Describe message for the statement
    frontend::describe(b'S', &name, conn.write_buf())?;

    // Send Sync to get responses
    frontend::sync(conn.write_buf());
    conn.flush()?;

    // Process responses
    let mut parsed_params = Vec::new();
    let mut parsed_columns = Vec::new();

    loop {
        let msg = conn.read_message()?;
        match msg {
            Message::ParseComplete => {
                // Statement parsed successfully
            }
            Message::ParameterDescription(desc) => {
                for oid in desc.parameters().filter_map(|r| {
                    r.map_err(|e| trace!(target: "hyperdb_api_core::client", error = %e, "dropped error parsing parameter OID")).ok()
                }) {
                    parsed_params.push(oid);
                }
            }
            Message::RowDescription(desc) => {
                for f in desc.fields().filter_map(|r| {
                    r.map_err(|e| trace!(target: "hyperdb_api_core::client", error = %e, "dropped error parsing row description field")).ok()
                }) {
                    parsed_columns.push(Column::new(
                        f.name().to_string(),
                        f.type_oid(),
                        f.type_modifier(),
                        super::statement::ColumnFormat::from_code(f.format()),
                    ));
                }
            }
            Message::NoData => {
                // Statement returns no data (e.g., INSERT)
            }
            Message::ReadyForQuery(_) => {
                break;
            }
            Message::ErrorResponse(body) => {
                return Err(conn.consume_error(&body));
            }
            _ => {}
        }
    }

    Ok(PreparedStatement {
        name,
        query: query.to_string(),
        param_types: parsed_params,
        columns: parsed_columns,
    })
}

/// Executes a prepared statement with parameters, collecting all rows.
///
/// **Every parameter is bound as PostgreSQL binary.** Values whose
/// [`ParamFormat`] is [`ParamFormat::Text`] — a scaled `NUMERIC`, a
/// `geography` — will be rejected by the server, because the bytes
/// `ToSqlParam::encode_param` produced for them are text. That combination is
/// unreachable from `hyperdb-api`, which streams through
/// [`RawConnection::start_execute_prepared_with_formats`] instead; if you are
/// calling this directly and need mixed formats, use
/// [`execute_prepared_no_result_with_formats`] or the streaming path.
///
/// # Errors
///
/// - Returns [`Error`] (connection) if the connection mutex is poisoned.
/// - Returns [`Error::Query`] if the server rejects `Bind` / `Execute`
///   (parameter type mismatch, constraint violation, etc.).
/// - Returns [`Error`] (I/O) / [`Error`] (closed) on wire-protocol I/O
///   failure.
/// - Propagates row-construction errors from `Row::new` if a
///   `DataRow` cannot be decoded against the prepared columns.
pub fn execute_prepared(
    connection: &Arc<Mutex<RawConnection<SyncStream>>>,
    statement: &PreparedStatement,
    params: &[Option<&[u8]>],
) -> Result<Vec<Row>> {
    // `&[]` means all-binary; see `bind_format_codes`. Infallible here.
    let param_formats = bind_format_codes(&[], params.len())?;
    let result_formats: Vec<i16> = vec![1; statement.columns.len()]; // 1 = binary

    let mut conn = connection
        .lock()
        .map_err(|_| Error::connection("connection mutex poisoned"))?;

    frontend::bind(
        "", // unnamed portal
        &statement.name,
        &param_formats,
        params,
        &result_formats,
        conn.write_buf(),
    )?;

    // Execute
    frontend::execute("", 0, conn.write_buf())?; // 0 = fetch all rows

    // Sync
    frontend::sync(conn.write_buf());
    conn.flush()?;

    // Process responses
    let mut rows = Vec::new();
    let columns = Arc::new(statement.columns.clone());

    loop {
        let msg = conn.read_message()?;
        match msg {
            Message::BindComplete => {
                // Bind succeeded
            }
            Message::DataRow(data) => {
                rows.push(Row::new(Arc::clone(&columns), data)?);
            }
            Message::CommandComplete(_) => {
                // Execution complete
            }
            Message::EmptyQueryResponse => {
                // Empty query
            }
            Message::ReadyForQuery(_) => {
                break;
            }
            Message::ErrorResponse(body) => {
                return Err(conn.consume_error(&body));
            }
            _ => {}
        }
    }

    Ok(rows)
}

/// Executes a prepared statement that doesn't return rows.
///
/// # Errors
///
/// Same failure modes as [`execute_prepared`] (minus row-construction
/// errors — this path never builds rows).
pub fn execute_prepared_no_result(
    connection: &Arc<Mutex<RawConnection<SyncStream>>>,
    statement: &PreparedStatement,
    params: &[Option<&[u8]>],
) -> Result<u64> {
    execute_prepared_no_result_with_formats(connection, statement, params, &[])
}

/// Executes a prepared statement that doesn't return rows, with a
/// caller-chosen wire format per parameter.
///
/// `param_formats` must be the same length as `params`, or empty to mean
/// "every parameter is binary".
///
/// # Errors
///
/// - Returns [`Error`] (protocol) if `param_formats` is non-empty and its
///   length differs from `params`.
/// - Otherwise the same failure modes as [`execute_prepared_no_result`].
pub fn execute_prepared_no_result_with_formats(
    connection: &Arc<Mutex<RawConnection<SyncStream>>>,
    statement: &PreparedStatement,
    params: &[Option<&[u8]>],
    param_formats: &[ParamFormat],
) -> Result<u64> {
    let param_format_codes = bind_format_codes(param_formats, params.len())?;

    let mut conn = connection
        .lock()
        .map_err(|_| Error::connection("connection mutex poisoned"))?;

    let result_formats: Vec<i16> = vec![];

    frontend::bind(
        "",
        &statement.name,
        &param_format_codes,
        params,
        &result_formats,
        conn.write_buf(),
    )?;

    // Execute
    frontend::execute("", 0, conn.write_buf())?;

    // Sync
    frontend::sync(conn.write_buf());
    conn.flush()?;

    // Process responses
    let mut affected_rows = 0u64;

    loop {
        let msg = conn.read_message()?;
        match msg {
            Message::BindComplete => {}
            Message::CommandComplete(body) => {
                if let Ok(tag) = body.tag() {
                    affected_rows = parse_affected_rows(tag);
                }
            }
            Message::EmptyQueryResponse => {}
            Message::ReadyForQuery(_) => {
                break;
            }
            Message::ErrorResponse(body) => {
                return Err(conn.consume_error(&body));
            }
            _ => {}
        }
    }

    Ok(affected_rows)
}

/// Closes a prepared statement on the server.
///
/// # Errors
///
/// - Returns [`Error`] (connection) if the connection mutex is poisoned.
/// - Returns [`Error::Query`] if the server reports an `ErrorResponse`
///   during `Close`/`Sync`.
/// - Returns [`Error`] (I/O) / [`Error`] (closed) on wire-protocol I/O
///   failure.
pub fn close_statement(
    connection: &Arc<Mutex<RawConnection<SyncStream>>>,
    statement: &PreparedStatement,
) -> Result<()> {
    close_statement_internal(connection, statement)
}

/// Internal close function that can be used from Drop.
fn close_statement_internal(
    connection: &Arc<Mutex<RawConnection<SyncStream>>>,
    statement: &PreparedStatement,
) -> Result<()> {
    let mut conn = connection
        .lock()
        .map_err(|_| Error::connection("connection mutex poisoned"))?;

    // Send Close message for the statement
    frontend::close(b'S', &statement.name, conn.write_buf())?;

    // Sync
    frontend::sync(conn.write_buf());
    conn.flush()?;

    // Process responses
    loop {
        let msg = conn.read_message()?;
        match msg {
            Message::CloseComplete => {}
            Message::ReadyForQuery(_) => {
                break;
            }
            Message::ErrorResponse(body) => {
                return Err(conn.consume_error(&body));
            }
            _ => {}
        }
    }

    Ok(())
}

/// Creates an owned prepared statement that automatically closes when dropped.
///
/// # Errors
///
/// Propagates any error from [`prepare`].
pub fn prepare_owned(
    connection: &Arc<Mutex<RawConnection<SyncStream>>>,
    query: &str,
    param_types: &[Oid],
) -> Result<OwnedPreparedStatement> {
    let statement = prepare(connection, query, param_types)?;
    Ok(OwnedPreparedStatement::new(statement, connection))
}

/// Parses affected row count from a command tag.
fn parse_affected_rows(tag: &str) -> u64 {
    let parts: Vec<&str> = tag.split_whitespace().collect();

    match parts.first() {
        Some(&"INSERT") => parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0),
        Some(&"UPDATE" | &"DELETE" | &"SELECT" | &"COPY") => {
            parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0)
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sql_param_i16() {
        assert_eq!(0_i16.encode(), vec![0, 0]);
        assert_eq!(1_i16.encode(), vec![1, 0]);
        assert_eq!((-1_i16).encode(), vec![255, 255]);
    }

    #[test]
    fn test_sql_param_i32() {
        assert_eq!(0_i32.encode(), vec![0, 0, 0, 0]);
        assert_eq!(1_i32.encode(), vec![1, 0, 0, 0]);
        assert_eq!((-1_i32).encode(), vec![255, 255, 255, 255]);
        assert_eq!(256_i32.encode(), vec![0, 1, 0, 0]);
    }

    #[test]
    fn test_sql_param_i64() {
        assert_eq!(0_i64.encode(), vec![0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(1_i64.encode(), vec![1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            (-1_i64).encode(),
            vec![255, 255, 255, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn test_sql_param_f32() {
        let encoded = 1.5_f32.encode();
        assert_eq!(encoded.len(), 4);
        let decoded = f32::from_le_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(decoded, 1.5);
    }

    #[test]
    fn test_sql_param_f64() {
        let encoded = 1.5_f64.encode();
        assert_eq!(encoded.len(), 8);
        let decoded = f64::from_le_bytes([
            encoded[0], encoded[1], encoded[2], encoded[3], encoded[4], encoded[5], encoded[6],
            encoded[7],
        ]);
        assert_eq!(decoded, 1.5);
    }

    #[test]
    fn test_sql_param_bool() {
        assert_eq!(true.encode(), vec![1]);
        assert_eq!(false.encode(), vec![0]);
    }

    #[test]
    fn test_sql_param_str() {
        assert_eq!("hello".encode(), b"hello".to_vec());
        assert_eq!("".encode(), Vec::<u8>::new());
        assert_eq!("héllo".encode(), "héllo".as_bytes().to_vec());
    }

    #[test]
    fn test_sql_param_string() {
        let s = String::from("hello");
        assert_eq!(s.encode(), b"hello".to_vec());
        assert_eq!(s.encode(), b"hello".to_vec());
    }

    #[test]
    fn test_sql_param_bytes() {
        let bytes: Vec<u8> = vec![1, 2, 3, 4];
        assert_eq!(bytes.encode(), vec![1, 2, 3, 4]);
        assert_eq!(bytes.as_slice().encode(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_params_macro_empty() {
        let p = params![];
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn test_params_macro_single() {
        let p = params![42_i32];
        assert_eq!(p.len(), 1);
        assert_eq!(p[0], Some(vec![42, 0, 0, 0]));
    }

    #[test]
    fn test_params_macro_multiple() {
        let p = params![42_i32, "hello", true];
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], Some(vec![42, 0, 0, 0]));
        assert_eq!(p[1], Some(b"hello".to_vec()));
        assert_eq!(p[2], Some(vec![1]));
    }
}
