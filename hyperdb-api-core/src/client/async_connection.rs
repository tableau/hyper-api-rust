// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async low-level connection handling.
//!
//! This module provides [`AsyncRawConnection`], the async version of [`RawConnection`](super::connection::RawConnection).
//! It uses tokio's async I/O traits for non-blocking network operations.

use std::collections::HashMap;

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info, warn};

use crate::protocol::message::{backend::Message, frontend};

use super::auth::{self, AuthState};
use super::error::{Error, Result};
use super::statement::{ParamFormat, bind_format_codes};

/// An async raw connection to a Hyper server.
///
/// This is the async equivalent of [`RawConnection`](super::connection::RawConnection),
/// using tokio's async I/O traits instead of std's sync I/O.
///
/// The connection is generic over the stream type `S`, allowing it to work
/// with different transport mechanisms (`TcpStream`, `TlsStream`, etc.) as long as they
/// implement `AsyncRead + AsyncWrite + Unpin`.
#[derive(Debug)]
pub struct AsyncRawConnection<S> {
    /// The underlying async I/O stream.
    stream: S,
    /// Buffer for reading incoming messages from the server.
    read_buf: BytesMut,
    /// Buffer for writing outgoing messages to the server.
    write_buf: BytesMut,
    /// Backend process ID (for cancel requests).
    process_id: i32,
    /// Secret key for authenticating cancel requests.
    secret_key: i32,
    /// Server parameters received during startup.
    server_params: HashMap<String, String>,
    /// Set by `AsyncCopyInWriter::Drop` when a COPY session is abandoned.
    /// The `CopyFail` message has been written to `write_buf` but not flushed.
    /// The next async operation must flush and drain the server response
    /// (`ErrorResponse` + `ReadyForQuery`) before proceeding.
    pending_copy_cancel: bool,
    /// Sticky flag mirroring
    /// [`RawConnection`](super::connection::RawConnection)'s
    /// `desynchronized` field. Set when a bounded drain exhausts its
    /// budget or hits a mid-drain I/O error; never cleared. See
    /// [`Self::is_healthy`] and [`Self::ensure_healthy`] for the
    /// consumer-facing API.
    desynchronized: bool,
    /// `true` while a write to the stream is in progress. It is set before
    /// the first `await` of a write and cleared only when the write
    /// completes, so it is still `true` if the future was dropped mid-write
    /// (cancellation) or the write failed. In that state a partial frame may
    /// be on the wire and nothing appended after it can be parsed as a
    /// message boundary, so the connection is unusable. See
    /// [`Self::is_healthy`].
    write_in_flight: bool,
}

impl<S> AsyncRawConnection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Creates a new async raw connection from a stream.
    ///
    /// Initializes read and write buffers with default capacity (64 KB each).
    /// The connection is not yet authenticated - call `startup()` to begin
    /// the connection handshake.
    pub fn new(stream: S) -> Self {
        AsyncRawConnection {
            stream,
            read_buf: BytesMut::with_capacity(64 * 1024),
            write_buf: BytesMut::with_capacity(64 * 1024),
            process_id: 0,
            secret_key: 0,
            server_params: HashMap::new(),
            pending_copy_cancel: false,
            desynchronized: false,
            write_in_flight: false,
        }
    }

    /// Returns `true` if this connection is still in a known-good state
    /// and safe to use for new requests. See
    /// [`super::connection::RawConnection::is_healthy`] for the full
    /// semantics — this is the async mirror with identical behavior.
    pub fn is_healthy(&self) -> bool {
        !self.desynchronized && !self.write_in_flight
    }

    /// Marks this connection as desynchronized.
    ///
    /// Used by async result streams that are dropped mid-iteration: the
    /// [`Drop`] impl cannot `await` to drain trailing `ErrorResponse +
    /// ReadyForQuery` messages after sending a cancel, so it flags the
    /// connection so the next operation short-circuits with a clear error
    /// rather than hanging or misinterpreting stale server output.
    pub fn mark_desynchronized(&mut self) {
        self.desynchronized = true;
    }

    /// Async mirror of
    /// [`super::connection::RawConnection::ensure_healthy`]. Called from
    /// the entry point of every `pub async fn` that initiates a new
    /// server request to short-circuit operations on a desynchronized
    /// connection before any bytes hit the wire.
    pub(crate) fn ensure_healthy(&self) -> Result<()> {
        if !self.is_healthy() {
            return Err(Error::connection(
                "connection is desynchronized from the server and cannot be reused; \
                 discard it and open a new one",
            ));
        }
        Ok(())
    }

    /// Returns the process ID assigned by the server.
    pub fn process_id(&self) -> i32 {
        self.process_id
    }

    /// Returns the secret key for cancel requests.
    pub fn secret_key(&self) -> i32 {
        self.secret_key
    }

    /// Returns a reference to the underlying stream.
    pub fn stream(&self) -> &S {
        &self.stream
    }

    /// Returns a mutable reference to the underlying stream.
    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Returns a server parameter value by name.
    pub fn parameter_status(&self, name: &str) -> Option<&str> {
        self.server_params
            .get(name)
            .map(std::string::String::as_str)
    }

    /// Queues a `CopyFail` message in the write buffer (synchronous).
    ///
    /// Called from `AsyncCopyInWriter::Drop` when a COPY session is abandoned
    /// without `finish()` or `cancel()`. The `CopyFail` is written to the buffer
    /// but NOT flushed (we can't do async I/O from `Drop`). The next async
    /// operation will call [`drain_pending_copy_cancel`](Self::drain_pending_copy_cancel) to flush and drain
    /// the server's `ErrorResponse` + `ReadyForQuery` before proceeding.
    ///
    /// If a write was interrupted (the COPY future was dropped or failed
    /// mid-`write_all`), a partial frame is on the wire and the server would
    /// read the `CopyFail` bytes as the rest of that frame, so it never sees
    /// the cancel. The `CopyFail` is not queued then; the connection is marked
    /// desynchronized instead, so the next operation fails fast rather than
    /// waiting for a `ReadyForQuery` that cannot arrive.
    pub fn queue_copy_fail(&mut self, reason: &str) {
        if self.write_in_flight {
            self.desynchronized = true;
            return;
        }
        frontend::copy_fail(reason, &mut self.write_buf);
        self.pending_copy_cancel = true;
    }

    /// Drains a pending COPY cancel that was queued by `queue_copy_fail()`.
    ///
    /// If `pending_copy_cancel` is set, this flushes the `CopyFail` message to
    /// the server and reads messages until `ReadyForQuery`, restoring the
    /// connection to a usable state. Called automatically at the start of
    /// new operations (`simple_query`, `query_binary`, `start_copy_in*`).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if flushing the queued `CopyFail` or
    /// reading the server's drain responses fails. A successful drain
    /// clears `pending_copy_cancel`.
    pub async fn drain_pending_copy_cancel(&mut self) -> Result<()> {
        if !self.pending_copy_cancel {
            return Ok(());
        }

        // Flush the queued CopyFail message
        self.flush().await?;

        // Drain messages (the expected ErrorResponse, notices) until the
        // connection is back in ReadyForQuery state. Bounded: a server that
        // never answers marks the connection desynchronized instead of
        // hanging every later operation.
        if self
            .drain_until_ready_bounded(super::connection::POST_ERROR_DRAIN_CAP)
            .await
        {
            self.pending_copy_cancel = false;
            debug!(
                target: "hyperdb_api_core::client",
                "drained pending COPY cancel — connection restored"
            );
            Ok(())
        } else {
            Err(Error::connection(
                "could not drain the abandoned COPY; connection marked desynchronized",
            ))
        }
    }

    /// Sends a startup message and performs initial handshake (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (auth) when the server requests an
    ///   auth method and no password is supplied, when the offered
    ///   SASL mechanisms exclude SCRAM-SHA-256, or when SCRAM state
    ///   is missing at the SASL-continue / SASL-final step.
    /// - Returns [`Error`] (server) when the server sends an `ErrorResponse`
    ///   during startup (unknown user, unknown database, etc.).
    /// - Returns [`Error`] (protocol) if a message arrives out of
    ///   sequence.
    /// - Returns [`Error`] (I/O) on transport read/write failure.
    pub async fn startup(&mut self, params: &[(&str, &str)], password: Option<&str>) -> Result<()> {
        // Send startup message
        frontend::startup_message(params, &mut self.write_buf)?;
        self.flush().await?;

        // Handle authentication
        let mut auth_state: Option<AuthState> = None;

        loop {
            let msg = self.read_message().await?;
            match msg {
                Message::AuthenticationOk => {
                    info!(target: "hyperdb_api", "connection-auth-success");
                }
                Message::AuthenticationCleartextPassword => {
                    debug!(target: "hyperdb_api", method = "cleartext", "connection-auth-method");
                    let password = password.ok_or_else(|| {
                        Error::authentication(
                            "server requested cleartext password but none provided",
                        )
                    })?;
                    frontend::password_message(password, &mut self.write_buf)?;
                    self.flush().await?;
                }
                Message::AuthenticationMd5Password(body) => {
                    debug!(target: "hyperdb_api", method = "MD5", "connection-auth-method");
                    let password = password.ok_or_else(|| {
                        Error::authentication("server requested MD5 password but none provided")
                    })?;
                    let user = params
                        .iter()
                        .find(|(k, _)| *k == "user")
                        .map_or("", |(_, v)| *v);

                    let md5_response = auth::compute_md5_password(user, password, &body.salt());
                    frontend::password_message(&md5_response, &mut self.write_buf)?;
                    self.flush().await?;
                }
                Message::AuthenticationSasl(body) => {
                    debug!(target: "hyperdb_api", method = "SCRAM-SHA-256", "connection-auth-method");
                    let password = password.ok_or_else(|| {
                        Error::authentication(
                            "server requested SASL authentication but no password provided",
                        )
                    })?;

                    let mechanisms: Vec<&str> = body.mechanisms().collect();
                    if !mechanisms.contains(&"SCRAM-SHA-256") {
                        return Err(Error::authentication(format!(
                            "server offered unsupported SASL mechanisms: {mechanisms:?}"
                        )));
                    }

                    let (state, client_first) = auth::scram_client_first(password)?;
                    auth_state = Some(state);

                    frontend::sasl_initial_response(
                        "SCRAM-SHA-256",
                        &client_first,
                        &mut self.write_buf,
                    )?;
                    self.flush().await?;
                }
                Message::AuthenticationSaslContinue(body) => {
                    let state = auth_state.take().ok_or_else(|| {
                        Error::authentication("received SASL continue without initial state")
                    })?;

                    let server_first = body.data();
                    let (new_state, client_final) = auth::scram_client_final(state, server_first)?;
                    auth_state = Some(new_state);

                    frontend::sasl_response(&client_final, &mut self.write_buf)?;
                    self.flush().await?;
                }
                Message::AuthenticationSaslFinal(body) => {
                    let state = auth_state.take().ok_or_else(|| {
                        Error::authentication("received SASL final without state")
                    })?;
                    auth::scram_verify_server(state, body.data())?;
                }
                Message::BackendKeyData(data) => {
                    self.process_id = data.process_id();
                    self.secret_key = data.secret_key();
                }
                Message::ParameterStatus(body) => {
                    if let (Ok(name), Ok(value)) = (body.name(), body.value()) {
                        self.server_params
                            .insert(name.to_string(), value.to_string());
                    }
                }
                Message::ReadyForQuery(_) => {
                    return Ok(());
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(&body).await);
                }
                _ => {
                    return Err(Error::protocol("unexpected message during startup"));
                }
            }
        }
    }

    /// Sends a simple query and returns all messages until `ReadyForQuery` (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection has been
    ///   marked unhealthy.
    /// - Returns [`Error`] (server) when the server emits an
    ///   `ErrorResponse` (SQL error, constraint violation, etc.).
    /// - Returns [`Error`] (I/O) / [`Error`] (closed) on transport
    ///   read/write failure.
    /// - Propagates any error from
    ///   [`Self::drain_pending_copy_cancel`] when a queued `CopyFail`
    ///   needs to be flushed first.
    pub async fn simple_query(&mut self, query: &str) -> Result<Vec<Message>> {
        self.ensure_healthy()?;
        self.drain_pending_copy_cancel().await?;
        frontend::query(query, &mut self.write_buf)?;
        self.flush().await?;

        let mut messages = Vec::new();
        loop {
            let msg = self.read_message().await?;
            match &msg {
                Message::ReadyForQuery(_) => {
                    messages.push(msg);
                    return Ok(messages);
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(body).await);
                }
                _ => {
                    messages.push(msg);
                }
            }
        }
    }

    /// Sends a query using extended protocol with binary format results (async).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::simple_query`].
    pub async fn query_binary(&mut self, query: &str) -> Result<Vec<Message>> {
        self.ensure_healthy()?;
        self.drain_pending_copy_cancel().await?;
        const HYPER_BINARY_FORMAT: i16 = 2;

        frontend::parse("", query, &[], &mut self.write_buf)?;
        frontend::bind(
            "",
            "",
            &[],
            &[],
            &[HYPER_BINARY_FORMAT],
            &mut self.write_buf,
        )?;
        frontend::describe(b'P', "", &mut self.write_buf)?;
        frontend::execute("", 0, &mut self.write_buf)?;
        frontend::sync(&mut self.write_buf);

        self.flush().await?;

        let mut messages = Vec::new();
        loop {
            let msg = self.read_message().await?;
            match &msg {
                Message::ReadyForQuery(_) => {
                    messages.push(msg);
                    return Ok(messages);
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(body).await);
                }
                _ => {
                    messages.push(msg);
                }
            }
        }
    }

    /// Starts a binary query but leaves result consumption to the caller (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (I/O) on transport write failure.
    /// - Propagates any error from [`Self::drain_pending_copy_cancel`].
    pub async fn start_query_binary(&mut self, query: &str) -> Result<()> {
        self.ensure_healthy()?;
        // Drain any CopyFail queued by `AsyncCopyInWriter::Drop` before
        // writing the extended-query bytes. Without this, the flush at
        // the end of this method would send [CopyFail | Parse | Bind |
        // Describe | Execute | Sync] in a single buffer and the server
        // would answer with CopyFail's ErrorResponse+ReadyForQuery
        // interleaved with our query's responses — the read loop would
        // then misattribute the COPY error to this query.
        self.drain_pending_copy_cancel().await?;
        const HYPER_BINARY_FORMAT: i16 = 2;

        frontend::parse("", query, &[], &mut self.write_buf)?;
        frontend::bind(
            "",
            "",
            &[],
            &[],
            &[HYPER_BINARY_FORMAT],
            &mut self.write_buf,
        )?;
        frontend::describe(b'P', "", &mut self.write_buf)?;
        frontend::execute("", 0, &mut self.write_buf)?;
        frontend::sync(&mut self.write_buf);

        self.flush().await
    }

    /// Starts a simple query but leaves result consumption to the caller (async).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::start_query_binary`].
    pub async fn start_simple_query(&mut self, query: &str) -> Result<()> {
        self.ensure_healthy()?;
        // See `start_query_binary` for why the pending-copy-cancel drain
        // is required before writing any new query bytes.
        self.drain_pending_copy_cancel().await?;
        frontend::query(query, &mut self.write_buf)?;
        self.flush().await
    }

    /// Starts an **execute** of a prepared statement but leaves result
    /// consumption to the caller (async).
    ///
    /// Async mirror of
    /// [`super::connection::RawConnection::start_execute_prepared`]. See
    /// that method's docs for the split format-code rationale (params
    /// use `1` = PG binary/BE, results use `2` = HyperBinary/LE).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (I/O) on transport write failure.
    /// - Propagates any error from [`Self::drain_pending_copy_cancel`].
    pub async fn start_execute_prepared(
        &mut self,
        statement_name: &str,
        params: &[Option<&[u8]>],
        column_count: usize,
    ) -> Result<()> {
        self.start_execute_prepared_with_formats(statement_name, params, &[], column_count)
            .await
    }

    /// Same as [`Self::start_execute_prepared`], but with a caller-chosen
    /// wire format per parameter.
    ///
    /// Async mirror of
    /// [`super::connection::RawConnection::start_execute_prepared_with_formats`].
    /// `param_formats` is either empty — meaning **every parameter is
    /// binary** — or the same length as `params`.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (protocol) if `param_formats` is non-empty and its
    ///   length differs from `params`.
    /// - Otherwise the same failure modes as
    ///   [`Self::start_execute_prepared`].
    pub async fn start_execute_prepared_with_formats(
        &mut self,
        statement_name: &str,
        params: &[Option<&[u8]>],
        param_formats: &[ParamFormat],
        column_count: usize,
    ) -> Result<()> {
        let codes = bind_format_codes(param_formats, params.len())?;
        self.bind_execute_sync(statement_name, params, &codes, column_count)
            .await
    }

    async fn bind_execute_sync(
        &mut self,
        statement_name: &str,
        params: &[Option<&[u8]>],
        param_format_codes: &[i16],
        column_count: usize,
    ) -> Result<()> {
        self.ensure_healthy()?;
        // Same rationale as `start_query_binary` for draining a pending
        // CopyFail before writing new extended-query bytes.
        self.drain_pending_copy_cancel().await?;

        const HYPER_BINARY_FORMAT: i16 = 2;
        let result_formats: Vec<i16> = vec![HYPER_BINARY_FORMAT; column_count];

        frontend::bind(
            "", // unnamed portal
            statement_name,
            param_format_codes,
            params,
            &result_formats,
            &mut self.write_buf,
        )?;
        frontend::execute("", 0, &mut self.write_buf)?;
        frontend::sync(&mut self.write_buf);

        self.flush().await
    }

    /// Reads a single message from the server (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (I/O) if reading from the transport fails or
    ///   if [`Message::parse`] reports a malformed frame.
    /// - Returns [`Error`] (closed) when the transport reaches EOF
    ///   (server closed the connection).
    pub async fn read_message(&mut self) -> Result<Message> {
        loop {
            if let Some(msg) = Message::parse(&mut self.read_buf).map_err(Error::from_io)? {
                return Ok(msg);
            }

            // Need more data — read directly into the spare capacity of
            // `read_buf`, no temporary buffer or `extend_from_slice` memcpy.
            // See the sync mirror in
            // [`super::connection::RawConnection::read_message`] for the
            // full rationale on the 64 KiB ceiling and Windows-loopback
            // syscall amplification.
            let prev_len = self.read_buf.len();
            self.read_buf.resize(prev_len + 64 * 1024, 0);
            let n = self.stream.read(&mut self.read_buf[prev_len..]).await?;
            if n == 0 {
                self.read_buf.truncate(prev_len);
                warn!(target: "hyperdb_api", "connection-closed");
                return Err(Error::closed("connection closed"));
            }
            self.read_buf.truncate(prev_len + n);
        }
    }

    /// Async equivalent of
    /// [`super::connection::RawConnection::drain_until_ready`]. Unbounded;
    /// prefer [`drain_until_ready_bounded`](Self::drain_until_ready_bounded)
    /// in destructors and other code paths where blocking indefinitely is
    /// unacceptable. Drain errors are logged via `tracing::warn!` and then
    /// swallowed.
    pub async fn drain_until_ready(&mut self) {
        let _ = self.drain_until_ready_bounded(usize::MAX).await;
    }

    /// Async equivalent of
    /// [`super::connection::RawConnection::drain_until_ready_bounded`].
    /// See that function's docs for the full semantics, including why we do
    /// **not** send a `Sync` before draining (it would produce an extra
    /// `ReadyForQuery` on the wire and corrupt the next query's response).
    pub async fn drain_until_ready_bounded(&mut self, max_messages: usize) -> bool {
        for i in 0..max_messages {
            match self.read_message().await {
                Ok(Message::ReadyForQuery(_)) => return true,
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        target: "hyperdb_api_core::client",
                        error = %e,
                        messages_read = i,
                        "drain_until_ready: read error mid-drain (likely closed connection); \
                         connection marked desynchronized",
                    );
                    // Mirror of sync path: any mid-drain read error leaves
                    // the connection in unknown state. See
                    // `super::connection::RawConnection::drain_until_ready_bounded`
                    // for the full rationale.
                    self.desynchronized = true;
                    return false;
                }
            }
        }
        warn!(
            target: "hyperdb_api_core::client",
            max_messages,
            "drain_until_ready_bounded: exhausted budget without seeing ReadyForQuery; \
             connection marked desynchronized and should not be reused",
        );
        self.desynchronized = true;
        false
    }

    /// Async equivalent of
    /// [`super::connection::RawConnection::consume_error`]. Parse the error
    /// body and drain the rest of the response in one call. Semantics are
    /// identical to the sync version, including the
    /// [`POST_ERROR_DRAIN_CAP`](super::connection::POST_ERROR_DRAIN_CAP)
    /// safety valve — see that function's docs for the rationale. Unbounded
    /// drain would be particularly dangerous here because a stalled read
    /// on the underlying async stream would hang the caller's future
    /// indefinitely with no observable symptom; the bounded drain turns
    /// that into a loud `tracing::warn!` plus a connection marked for
    /// reconnect on next use.
    pub async fn consume_error(
        &mut self,
        body: &crate::protocol::message::backend::ErrorResponseBody,
    ) -> Error {
        let err = super::connection::parse_error_response(body);
        let _ = self
            .drain_until_ready_bounded(super::connection::POST_ERROR_DRAIN_CAP)
            .await;
        err
    }

    /// Flushes the write buffer to the server (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if writing the buffered bytes or flushing
    /// the underlying async transport fails.
    pub async fn flush(&mut self) -> Result<()> {
        if !self.write_buf.is_empty() {
            // Left set if this future is dropped or fails mid-write.
            self.write_in_flight = true;
            self.stream.write_all(&self.write_buf).await?;
            self.stream.flush().await?;
            self.write_buf.clear();
            self.write_in_flight = false;
        }
        Ok(())
    }

    /// Sends a terminate message and closes the connection (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if writing the `Terminate` frame or
    /// flushing the async transport fails.
    pub async fn terminate(&mut self) -> Result<()> {
        frontend::terminate(&mut self.write_buf);
        self.flush().await
    }

    /// Returns a mutable reference to the write buffer.
    pub fn write_buf(&mut self) -> &mut BytesMut {
        &mut self.write_buf
    }

    /// Initiates a COPY IN operation with `HyperBinary` format (async).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::start_copy_in_with_format`].
    pub async fn start_copy_in(&mut self, table_name: &str, columns: &[&str]) -> Result<()> {
        self.start_copy_in_with_format(table_name, columns, "HYPERBINARY")
            .await
    }

    /// Initiates a COPY IN operation with a specified format (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection has been
    ///   marked unhealthy.
    /// - Returns [`Error`] (server) if the server rejects the generated
    ///   `COPY ... FROM STDIN` statement.
    /// - Returns [`Error`] (I/O) on transport read/write failure.
    /// - Propagates any error from [`Self::drain_pending_copy_cancel`].
    pub async fn start_copy_in_with_format(
        &mut self,
        table_name: &str,
        columns: &[&str],
        format: &str,
    ) -> Result<()> {
        self.ensure_healthy()?;
        self.drain_pending_copy_cancel().await?;
        let column_list = if columns.is_empty() {
            String::new()
        } else {
            format!(
                " ({})",
                columns
                    .iter()
                    .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        let query = format!("COPY {table_name}{column_list} FROM STDIN WITH (FORMAT {format})");

        frontend::query(&query, &mut self.write_buf)?;
        self.flush().await?;

        loop {
            let msg = self.read_message().await?;
            match msg {
                Message::CopyInResponse(_) => {
                    return Ok(());
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(&body).await);
                }
                _ => {}
            }
        }
    }

    /// Sends COPY data to the server (sync - just buffers).
    ///
    /// # Errors
    ///
    /// Currently infallible — frame construction is pure. The `Result`
    /// return type is preserved for forward compatibility.
    pub fn send_copy_data(&mut self, data: &[u8]) -> Result<()> {
        frontend::copy_data(data, &mut self.write_buf);
        Ok(())
    }

    /// Sends COPY data directly to the stream without internal buffering (async).
    ///
    /// This writes the `CopyData` message directly to the TCP stream, letting
    /// the kernel's TCP stack handle buffering. Use `flush_stream()` periodically
    /// to ensure data is sent.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (protocol) if `data.len() + 4` exceeds
    ///   `u32::MAX` (PostgreSQL's per-message length cap).
    /// - Returns [`Error`] (I/O) if flushing buffered bytes or writing
    ///   the header / payload to the async transport fails.
    pub async fn send_copy_data_direct(&mut self, data: &[u8]) -> Result<()> {
        // Write CopyData message header + data directly to stream
        // Message format: 'd' (1 byte) + length (4 bytes BigEndian) + data
        let msg_len = u32::try_from(4 + data.len())
            .map_err(|_| Error::protocol("CopyData payload exceeds u32::MAX bytes"))?;
        let len_be = msg_len.to_be_bytes();
        let header = [b'd', len_be[0], len_be[1], len_be[2], len_be[3]];

        // `write_all` is not cancel-safe: if this future is dropped between
        // the awaits below, a partial frame stays on the wire. The flag is
        // left set in that case (see `write_in_flight`).
        self.write_in_flight = true;
        // First flush any pending buffered data
        if !self.write_buf.is_empty() {
            self.stream.write_all(&self.write_buf).await?;
            self.write_buf.clear();
        }
        self.stream.write_all(&header).await?;
        self.stream.write_all(data).await?;
        self.write_in_flight = false;
        Ok(())
    }

    /// Flushes the TCP stream without clearing the write buffer (async).
    ///
    /// Use this with `send_copy_data_direct()` to periodically ensure
    /// data is sent to the server.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if flushing the underlying async transport
    /// fails.
    pub async fn flush_stream(&mut self) -> Result<()> {
        self.stream.flush().await?;
        Ok(())
    }

    /// Finishes a COPY IN operation successfully (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (server) when the server emits an
    ///   `ErrorResponse` during finalization (for example, a
    ///   constraint violation from the accumulated rows).
    /// - Returns [`Error`] (I/O) / [`Error`] (closed) on transport
    ///   read/write failure.
    pub async fn finish_copy(&mut self) -> Result<u64> {
        self.ensure_healthy()?;
        self.flush().await?;

        frontend::copy_done(&mut self.write_buf);
        self.flush().await?;

        let mut row_count = 0u64;
        loop {
            let msg = self.read_message().await?;
            match msg {
                Message::CommandComplete(body) => {
                    if let Ok(tag) = body.tag()
                        && let Some(count_str) = tag.strip_prefix("COPY ")
                        && let Ok(count) = count_str.trim().parse()
                    {
                        row_count = count;
                    }
                }
                Message::ReadyForQuery(_) => {
                    return Ok(row_count);
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(&body).await);
                }
                _ => {}
            }
        }
    }

    /// Cancels a COPY IN operation (async).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] (I/O) if flushing the buffer or writing the
    /// `CopyFail` frame fails, or [`Error`] (closed) if the server
    /// drops the connection before returning `ReadyForQuery`.
    pub async fn cancel_copy(&mut self, reason: &str) -> Result<()> {
        self.ensure_healthy()?;
        self.flush().await?;

        frontend::copy_fail(reason, &mut self.write_buf);
        self.flush().await?;

        loop {
            let msg = self.read_message().await?;
            match msg {
                Message::ReadyForQuery(_) => {
                    return Ok(());
                }
                Message::ErrorResponse(_) => {}
                _ => {}
            }
        }
    }

    /// Executes a COPY ... TO STDOUT query and returns all output data (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (server) when the server rejects the COPY TO
    ///   STDOUT statement via `ErrorResponse`.
    /// - Returns [`Error`] (I/O) / [`Error`] (closed) on transport
    ///   read/write failure.
    pub async fn copy_out(&mut self, query: &str) -> Result<Vec<u8>> {
        self.ensure_healthy()?;
        self.drain_pending_copy_cancel().await?;
        frontend::query(query, &mut self.write_buf)?;
        self.flush().await?;

        let mut data = Vec::new();
        let mut in_copy_out = false;

        loop {
            let msg = self.read_message().await?;
            match msg {
                Message::CopyOutResponse(_) => {
                    in_copy_out = true;
                }
                Message::CopyData(body) if in_copy_out => {
                    data.extend_from_slice(body.data());
                }
                Message::CopyDone => {
                    in_copy_out = false;
                }
                Message::CommandComplete(_) => {}
                Message::ReadyForQuery(_) => {
                    return Ok(data);
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(&body).await);
                }
                _ => {}
            }
        }
    }

    /// Prepares a statement using the extended query protocol (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (server) if the server rejects the `Parse`
    ///   request (SQL syntax error, unknown type OIDs, etc.).
    /// - Returns [`Error`] (I/O) on transport read/write failure.
    pub async fn prepare(
        &mut self,
        name: &str,
        query: &str,
        param_types: &[crate::types::Oid],
    ) -> Result<(Vec<crate::types::Oid>, Vec<super::statement::Column>)> {
        use super::statement::{Column, ColumnFormat};

        self.ensure_healthy()?;
        self.drain_pending_copy_cancel().await?;

        // Send Parse message
        frontend::parse(name, query, param_types, &mut self.write_buf)?;

        // Send Describe message for the statement
        frontend::describe(b'S', name, &mut self.write_buf)?;

        // Send Sync to get responses
        frontend::sync(&mut self.write_buf);
        self.flush().await?;

        // Process responses
        let mut parsed_params = Vec::new();
        let mut parsed_columns = Vec::new();

        loop {
            let msg = self.read_message().await?;
            match msg {
                Message::ParseComplete => {}
                Message::ParameterDescription(desc) => {
                    for oid in desc.parameters().filter_map(std::result::Result::ok) {
                        parsed_params.push(oid);
                    }
                }
                Message::RowDescription(desc) => {
                    for f in desc.fields().filter_map(std::result::Result::ok) {
                        parsed_columns.push(Column::new(
                            f.name().to_string(),
                            f.type_oid(),
                            f.type_modifier(),
                            ColumnFormat::from_code(f.format()),
                        ));
                    }
                }
                Message::NoData => {}
                Message::ReadyForQuery(_) => {
                    break;
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(&body).await);
                }
                _ => {}
            }
        }

        Ok((parsed_params, parsed_columns))
    }

    /// Executes a prepared statement with parameters (async), collecting all
    /// rows.
    ///
    /// **Every parameter is bound as PostgreSQL binary.** Values whose
    /// [`ParamFormat`] is [`ParamFormat::Text`] — a scaled `NUMERIC`, a
    /// `geography` — will be rejected by the server, because the bytes
    /// `ToSqlParam::encode_param` produced for them are text. That
    /// combination is unreachable from `hyperdb-api`, which streams through
    /// [`Self::start_execute_prepared_with_formats`] instead; if you are
    /// calling this directly and need mixed formats, use
    /// [`Self::execute_prepared_no_result_with_formats`] or the streaming
    /// path.
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (server) if `Bind` / `Execute` fails on the
    ///   server (parameter type mismatch, constraint violation, etc.).
    /// - Returns [`Error`] (I/O) / [`Error`] (closed) on transport
    ///   read/write failure.
    /// - Propagates row-construction errors from
    ///   `super::row::Row::new` if a `DataRow` cannot be decoded
    ///   against the reported `RowDescription`.
    pub async fn execute_prepared(
        &mut self,
        statement_name: &str,
        params: &[Option<&[u8]>],
        column_count: usize,
    ) -> Result<Vec<super::row::Row>> {
        use super::statement::Column;
        use std::sync::Arc;

        self.ensure_healthy()?;
        // Prepared-statement execution writes Bind/Execute/Sync into the
        // buffer and flushes at the end; a pending CopyFail would be
        // flushed together with our bind bytes and corrupt the response
        // stream. See `start_query_binary` for the full argument.
        self.drain_pending_copy_cancel().await?;
        // `&[]` means all-binary; see `bind_format_codes`. Infallible here.
        let param_formats = bind_format_codes(&[], params.len())?;
        let result_formats: Vec<i16> = vec![1; column_count];

        frontend::bind(
            "",
            statement_name,
            &param_formats,
            params,
            &result_formats,
            &mut self.write_buf,
        )?;

        frontend::execute("", 0, &mut self.write_buf)?;
        frontend::sync(&mut self.write_buf);
        self.flush().await?;

        let mut rows = Vec::new();
        let mut columns: Option<Arc<Vec<Column>>> = None;

        loop {
            let msg = self.read_message().await?;
            match msg {
                Message::BindComplete => {}
                Message::RowDescription(desc) => {
                    let mut cols = Vec::new();
                    for f in desc.fields().filter_map(std::result::Result::ok) {
                        cols.push(Column::new(
                            f.name().to_string(),
                            f.type_oid(),
                            f.type_modifier(),
                            super::statement::ColumnFormat::from_code(f.format()),
                        ));
                    }
                    columns = Some(Arc::new(cols));
                }
                Message::DataRow(data) => {
                    if let Some(ref cols) = columns {
                        rows.push(super::row::Row::new(Arc::clone(cols), data)?);
                    }
                }
                Message::CommandComplete(_) => {}
                Message::EmptyQueryResponse => {}
                Message::ReadyForQuery(_) => {
                    break;
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(&body).await);
                }
                _ => {}
            }
        }

        Ok(rows)
    }

    /// Executes a prepared statement that doesn't return rows (async).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Self::execute_prepared`] (excluding
    /// row-construction errors — this path never builds rows).
    pub async fn execute_prepared_no_result(
        &mut self,
        statement_name: &str,
        params: &[Option<&[u8]>],
    ) -> Result<u64> {
        self.execute_prepared_no_result_with_formats(statement_name, params, &[])
            .await
    }

    /// Same as [`Self::execute_prepared_no_result`], but with a
    /// caller-chosen wire format per parameter.
    ///
    /// `param_formats` must be the same length as `params`, or empty to mean
    /// "every parameter is binary".
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (protocol) if `param_formats` is non-empty and its
    ///   length differs from `params`.
    /// - Otherwise the same failure modes as
    ///   [`Self::execute_prepared_no_result`].
    pub async fn execute_prepared_no_result_with_formats(
        &mut self,
        statement_name: &str,
        params: &[Option<&[u8]>],
        param_formats: &[ParamFormat],
    ) -> Result<u64> {
        self.ensure_healthy()?;
        // See `execute_prepared` and `start_query_binary` for why we must
        // drain any pending COPY cancel before writing new bytes.
        self.drain_pending_copy_cancel().await?;
        let param_format_codes = bind_format_codes(param_formats, params.len())?;
        let result_formats: Vec<i16> = vec![];

        frontend::bind(
            "",
            statement_name,
            &param_format_codes,
            params,
            &result_formats,
            &mut self.write_buf,
        )?;

        frontend::execute("", 0, &mut self.write_buf)?;
        frontend::sync(&mut self.write_buf);
        self.flush().await?;

        let mut affected_rows = 0u64;

        loop {
            let msg = self.read_message().await?;
            match msg {
                Message::BindComplete => {}
                Message::CommandComplete(body) => {
                    if let Ok(tag) = body.tag() {
                        // Parse formats like "INSERT 0 5", "UPDATE 10", "DELETE 3"
                        let parts: Vec<&str> = tag.split_whitespace().collect();
                        match parts.first() {
                            Some(&"INSERT") => {
                                if let Some(count) = parts.get(2) {
                                    affected_rows = count.parse().unwrap_or(0);
                                }
                            }
                            Some(&"UPDATE" | &"DELETE" | &"SELECT" | &"COPY") => {
                                if let Some(count) = parts.get(1) {
                                    affected_rows = count.parse().unwrap_or(0);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Message::EmptyQueryResponse => {}
                Message::ReadyForQuery(_) => {
                    break;
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(&body).await);
                }
                _ => {}
            }
        }

        Ok(affected_rows)
    }

    /// Closes a prepared statement (async).
    ///
    /// # Errors
    ///
    /// - Returns [`Error`] (connection) if the connection is unhealthy.
    /// - Returns [`Error`] (server) if the server reports an `ErrorResponse`
    ///   during `Close`/`Sync`.
    /// - Returns [`Error`] (I/O) / [`Error`] (closed) on transport
    ///   read/write failure.
    /// - Propagates any error from [`Self::drain_pending_copy_cancel`].
    pub async fn close_statement(&mut self, statement_name: &str) -> Result<()> {
        self.ensure_healthy()?;
        // Close + Sync get flushed together; a pending CopyFail would
        // share the flush and corrupt the response stream. See
        // `start_query_binary` for the full argument.
        self.drain_pending_copy_cancel().await?;
        frontend::close(b'S', statement_name, &mut self.write_buf)?;
        frontend::sync(&mut self.write_buf);
        self.flush().await?;

        loop {
            let msg = self.read_message().await?;
            match msg {
                Message::CloseComplete => {}
                Message::ReadyForQuery(_) => {
                    return Ok(());
                }
                Message::ErrorResponse(body) => {
                    return Err(self.consume_error(&body).await);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write future dropped mid-`write_all` (cancellation) must poison the
    /// connection. The peer end of the duplex pipe is never read, and its
    /// 8-byte capacity guarantees `write_all` parks after a partial write.
    #[tokio::test]
    async fn cancelled_copy_write_marks_connection_unhealthy() {
        let (client, _peer) = tokio::io::duplex(8);
        let mut conn = AsyncRawConnection::new(client);
        assert!(conn.is_healthy());

        let payload = [0u8; 1024];
        tokio::select! {
            biased;
            res = conn.send_copy_data_direct(&payload) => {
                panic!("write into a full pipe cannot complete: {res:?}");
            }
            () = std::future::ready(()) => {}
        }

        assert!(
            !conn.is_healthy(),
            "an interrupted write must poison the connection"
        );
        assert!(conn.ensure_healthy().is_err());
    }

    /// With a partial frame on the wire, a queued `CopyFail` would be read by
    /// the server as part of that frame and never seen. `queue_copy_fail`
    /// must not queue it and must leave the connection desynchronized.
    #[tokio::test]
    async fn queue_copy_fail_after_interrupted_write_desynchronizes() {
        let (client, _peer) = tokio::io::duplex(8);
        let mut conn = AsyncRawConnection::new(client);

        tokio::select! {
            biased;
            _ = conn.send_copy_data_direct(&[0u8; 1024]) => unreachable!(),
            () = std::future::ready(()) => {}
        }
        conn.queue_copy_fail("dropped");

        assert!(
            !conn.pending_copy_cancel,
            "CopyFail must not follow a partial frame"
        );
        assert!(conn.desynchronized);
    }

    /// A completed write leaves the connection healthy and `queue_copy_fail`
    /// behaves as before.
    #[tokio::test]
    async fn completed_copy_write_stays_healthy() {
        let (client, mut peer) = tokio::io::duplex(4096);
        let mut conn = AsyncRawConnection::new(client);

        conn.send_copy_data_direct(b"abc").await.unwrap();
        assert!(conn.is_healthy());

        conn.queue_copy_fail("abandoned");
        assert!(conn.pending_copy_cancel);
        assert!(conn.is_healthy());

        // 'd' + len(7) + payload reached the peer intact.
        let mut got = [0u8; 8];
        tokio::io::AsyncReadExt::read_exact(&mut peer, &mut got)
            .await
            .unwrap();
        assert_eq!(&got, &[b'd', 0, 0, 0, 7, b'a', b'b', b'c']);
    }
}
