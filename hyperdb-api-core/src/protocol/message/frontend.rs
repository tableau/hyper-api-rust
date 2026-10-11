// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Frontend (client-to-server) messages.
//!
//! Each function in this module writes a single `PostgreSQL` wire protocol
//! message into a [`BytesMut`] buffer. All structural fields (tags, lengths,
//! counts) are written in **`BigEndian`** per the `PostgreSQL` specification.
//!
//! The functions are intentionally stateless -- they append bytes to a buffer
//! and do not manage connection state. The connection layer in `hyper-client`
//! is responsible for sequencing messages correctly (e.g. Parse/Bind/Execute
//! followed by Sync).
//!
//! # Attribution
//!
//! Portions of this module were adapted from
//! [`postgres-protocol`](https://github.com/sfackler/rust-postgres)'s
//! `message/frontend.rs` (Copyright (c) 2016 Steven Fackler, MIT or
//! Apache-2.0). Adapted material includes the PostgreSQL v3.0
//! protocol-version literal (`196608`, `(3 << 16) | 0`), the startup-message
//! framing layout, and the `msg_len` helper. Hyper-specific changes added
//! on top include performance work and Hyper-specific message variants.
//! See the `NOTICE` file at the repo root for the full upstream copyright
//! and reproduced license text.

use bytes::{BufMut, BytesMut};
use std::io;

use crate::types::Oid;

/// Narrows a `usize` length to the `PostgreSQL` wire-protocol `i32` length prefix.
///
/// Panics on overflow. The `PostgreSQL` protocol caps individual messages at
/// `i32::MAX` bytes (~2 GiB); any single string, query, or byte slice exceeding
/// that is a programming error, not a runtime-recoverable condition. Per the
/// M-PANIC-ON-BUG guideline, we panic rather than threading a `Result` through
/// every wire-format helper.
#[inline]
fn msg_len(n: usize) -> i32 {
    i32::try_from(n).expect("PostgreSQL wire message field exceeds i32::MAX bytes")
}

#[expect(
    clippy::similar_names,
    reason = "paired bindings (request/response, reader/writer, etc.) are more readable with symmetric names than artificially distinct ones"
)]
/// Narrows a `usize` count to the `PostgreSQL` wire-protocol 16-bit parameter /
/// format count.
///
/// The wire field is `Int16` (signed on the wire, but values `0..=65_535` are
/// legal — the high bit is a permitted value, not a sign). We go through `u16`
/// first to trap genuine overflow, then bit-reinterpret to `i16` so
/// [`bytes::BufMut::put_i16`] writes the expected `BigEndian` representation.
///
/// Panics if the count exceeds `65_535`. In practice Hyper/PostgreSQL cap this
/// far lower (1600 parameters per query), so overflow is a programming error.
#[inline]
fn msg_count(n: usize) -> i16 {
    let as_u16 = u16::try_from(n)
        .expect("PostgreSQL wire message count exceeds u16::MAX (max 65_535 parameters)");
    // Bit-pattern reinterpret: `put_i16` writes the same 16 bits regardless of
    // signed/unsigned interpretation.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "intentional bit-pattern reinterpret; wire field is semantically 0..=65_535"
    )]
    let as_i16 = as_u16 as i16;
    as_i16
}

/// Writes a startup message to the buffer.
///
/// The startup message establishes the connection and sets initial parameters.
/// This is the first message sent by the client after establishing a TCP connection.
///
/// # Arguments
///
/// * `parameters` - Key-value pairs of connection parameters (e.g., "user", "database")
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility so that future validation
/// (e.g. parameter-length checks) can surface errors without a breaking
/// signature change.
///
/// # Panics
///
/// Panics via `msg_len` if the total encoded message length exceeds
/// `i32::MAX` bytes (~2 GiB). PostgreSQL caps messages at that size, so
/// exceeding it is a programming error.
pub fn startup_message(parameters: &[(&str, &str)], buf: &mut BytesMut) -> io::Result<()> {
    // Reserve space for length
    let len_idx = buf.len();
    buf.put_i32(0);

    // Protocol version 3.0
    buf.put_i32(196608); // (3 << 16) | 0

    // Parameters
    for (name, value) in parameters {
        buf.put_slice(name.as_bytes());
        buf.put_u8(0);
        buf.put_slice(value.as_bytes());
        buf.put_u8(0);
    }

    // Terminator
    buf.put_u8(0);

    // Update length
    let len = msg_len(buf.len() - len_idx);
    buf[len_idx..len_idx + 4].copy_from_slice(&len.to_be_bytes());

    Ok(())
}

/// Writes a password message (for authentication).
///
/// Sent in response to `AuthenticationCleartextPassword` or after MD5 hashing.
///
/// # Arguments
///
/// * `password` - The password to send (plaintext or MD5 hash)
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility.
///
/// # Panics
///
/// Panics via `msg_len` if `password.len()` exceeds `i32::MAX` bytes.
pub fn password_message(password: &str, buf: &mut BytesMut) -> io::Result<()> {
    buf.put_u8(b'p');
    buf.put_i32(4 + msg_len(password.len()) + 1);
    buf.put_slice(password.as_bytes());
    buf.put_u8(0);
    Ok(())
}

/// Writes a SASL initial response message.
///
/// Used to initiate SASL authentication (e.g., SCRAM-SHA-256).
/// Sent in response to `AuthenticationSasl` message.
///
/// # Arguments
///
/// * `mechanism` - The SASL mechanism name to use
/// * `data` - Initial client response data (may be empty)
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility.
///
/// # Panics
///
/// Panics via `msg_len` if `mechanism.len()` or `data.len()` exceeds
/// `i32::MAX` bytes.
pub fn sasl_initial_response(mechanism: &str, data: &[u8], buf: &mut BytesMut) -> io::Result<()> {
    buf.put_u8(b'p');
    let len = 4 + msg_len(mechanism.len()) + 1 + 4 + msg_len(data.len());
    buf.put_i32(len);
    buf.put_slice(mechanism.as_bytes());
    buf.put_u8(0);
    buf.put_i32(msg_len(data.len()));
    buf.put_slice(data);
    Ok(())
}

/// Writes a SASL response message.
///
/// Used to continue SASL authentication exchange.
/// Sent in response to `AuthenticationSaslContinue` messages.
///
/// # Arguments
///
/// * `data` - Client response data for this authentication step
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility.
///
/// # Panics
///
/// Panics via `msg_len` if `data.len()` exceeds `i32::MAX` bytes.
pub fn sasl_response(data: &[u8], buf: &mut BytesMut) -> io::Result<()> {
    buf.put_u8(b'p');
    buf.put_i32(4 + msg_len(data.len()));
    buf.put_slice(data);
    Ok(())
}

/// Writes a simple query message.
///
/// Executes a SQL query directly without using prepared statements.
/// The server will respond with `RowDescription`, `DataRow`, `CommandComplete`, etc.
///
/// # Arguments
///
/// * `query` - SQL query string (null-terminated)
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility.
///
/// # Panics
///
/// Panics via `msg_len` if `query.len()` exceeds `i32::MAX` bytes
/// (PostgreSQL's per-message cap).
pub fn query(query: &str, buf: &mut BytesMut) -> io::Result<()> {
    buf.put_u8(b'Q');
    buf.put_i32(4 + msg_len(query.len()) + 1);
    buf.put_slice(query.as_bytes());
    buf.put_u8(0);
    Ok(())
}

/// Writes a parse message (prepare statement).
///
/// Prepares a SQL statement for later execution. The server responds with
/// `ParseComplete` and may send `ParameterDescription`.
///
/// # Arguments
///
/// * `name` - Statement name (null-terminated, empty string for unnamed)
/// * `query` - SQL query string with parameter placeholders ($1, $2, etc.)
/// * `param_types` - OIDs of parameter types (empty if types should be inferred)
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility.
///
/// # Panics
///
/// - Panics via `msg_len` if `name.len()`, `query.len()`, or the encoded
///   message length exceeds `i32::MAX` bytes.
/// - Panics via `msg_count` if `param_types.len()` exceeds `u16::MAX`
///   (65 535). PostgreSQL caps parameters per query at 1 600, so overflow
///   is a programming error.
pub fn parse(name: &str, query: &str, param_types: &[Oid], buf: &mut BytesMut) -> io::Result<()> {
    buf.put_u8(b'P');

    let len = 4  // length itself
        + msg_len(name.len()) + 1  // statement name
        + msg_len(query.len()) + 1  // query string
        + 2  // parameter count
        + (msg_len(param_types.len()) * 4); // parameter OIDs

    buf.put_i32(len);
    buf.put_slice(name.as_bytes());
    buf.put_u8(0);
    buf.put_slice(query.as_bytes());
    buf.put_u8(0);
    buf.put_i16(msg_count(param_types.len()));
    for oid in param_types {
        buf.put_u32(oid.value());
    }

    Ok(())
}

/// Writes a bind message.
///
/// Binds parameter values to a prepared statement, creating a portal.
/// The server responds with `BindComplete`.
///
/// # Arguments
///
/// * `portal` - Portal name (null-terminated, empty string for unnamed)
/// * `statement` - Statement name (from Parse message)
/// * `param_formats` - Format codes for each parameter (0 = text, 1 = binary)
/// * `params` - Parameter values (None for NULL)
/// * `result_formats` - Format codes for result columns (0 = text, 1 = binary)
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility.
///
/// # Panics
///
/// - Panics via `msg_len` if any string length, parameter length, or the
///   total encoded message length exceeds `i32::MAX` bytes.
/// - Panics via `msg_count` if `param_formats.len()`, `params.len()`, or
///   `result_formats.len()` exceeds `u16::MAX` (65 535).
pub fn bind(
    portal: &str,
    statement: &str,
    param_formats: &[i16],
    params: &[Option<&[u8]>],
    result_formats: &[i16],
    buf: &mut BytesMut,
) -> io::Result<()> {
    buf.put_u8(b'B');

    // Calculate length
    let mut len = 4  // length itself
        + msg_len(portal.len()) + 1  // portal name
        + msg_len(statement.len()) + 1  // statement name
        + 2  // parameter format count
        + (msg_len(param_formats.len()) * 2)  // format codes
        + 2; // parameter value count

    for param in params {
        len += 4; // length field
        if let Some(data) = param {
            len += msg_len(data.len());
        }
    }

    len += 2; // result format count
    len += (msg_len(result_formats.len())) * 2; // result format codes

    buf.put_i32(len);
    buf.put_slice(portal.as_bytes());
    buf.put_u8(0);
    buf.put_slice(statement.as_bytes());
    buf.put_u8(0);

    // Parameter formats
    buf.put_i16(msg_count(param_formats.len()));
    for &format in param_formats {
        buf.put_i16(format);
    }

    // Parameter values
    buf.put_i16(msg_count(params.len()));
    for param in params {
        match param {
            Some(data) => {
                buf.put_i32(msg_len(data.len()));
                buf.put_slice(data);
            }
            None => {
                buf.put_i32(-1); // NULL
            }
        }
    }

    // Result formats
    buf.put_i16(msg_count(result_formats.len()));
    for &format in result_formats {
        buf.put_i16(format);
    }

    Ok(())
}

/// Writes a describe message.
///
/// Requests metadata about a prepared statement or portal.
/// The server responds with `RowDescription` or `ParameterDescription`.
///
/// # Arguments
///
/// * `kind` - 'S' for statement, 'P' for portal
/// * `name` - Statement or portal name (null-terminated, empty string for unnamed)
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility.
///
/// # Panics
///
/// Panics via `msg_len` if `name.len()` exceeds `i32::MAX` bytes.
pub fn describe(kind: u8, name: &str, buf: &mut BytesMut) -> io::Result<()> {
    buf.put_u8(b'D');
    buf.put_i32(4 + 1 + msg_len(name.len()) + 1);
    buf.put_u8(kind); // 'S' for statement, 'P' for portal
    buf.put_slice(name.as_bytes());
    buf.put_u8(0);
    Ok(())
}

/// Writes an execute message.
///
/// Executes a portal (bound statement). The server responds with `DataRow`
/// messages and `CommandComplete`. Use 0 for `max_rows` to fetch all rows.
///
/// # Arguments
///
/// * `portal` - Portal name (null-terminated, empty string for unnamed)
/// * `max_rows` - Maximum number of rows to return (0 = unlimited)
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility.
///
/// # Panics
///
/// Panics via `msg_len` if `portal.len()` exceeds `i32::MAX` bytes.
pub fn execute(portal: &str, max_rows: i32, buf: &mut BytesMut) -> io::Result<()> {
    buf.put_u8(b'E');
    buf.put_i32(4 + msg_len(portal.len()) + 1 + 4);
    buf.put_slice(portal.as_bytes());
    buf.put_u8(0);
    buf.put_i32(max_rows);
    Ok(())
}

/// Writes a sync message.
///
/// Forces the server to process all pending messages and respond with `ReadyForQuery`.
/// Should be sent after completing a query sequence (Parse/Bind/Execute).
pub fn sync(buf: &mut BytesMut) {
    buf.put_u8(b'S');
    buf.put_i32(4);
}

/// Writes a flush message.
///
/// Requests that the server flush its output buffer.
/// The server will send any pending messages but does not send a response.
pub fn flush(buf: &mut BytesMut) {
    buf.put_u8(b'H');
    buf.put_i32(4);
}

/// Writes a close message.
///
/// Closes a prepared statement or portal. The server responds with `CloseComplete`.
///
/// # Arguments
///
/// * `kind` - 'S' for statement, 'P' for portal
/// * `name` - Statement or portal name (null-terminated, empty string for unnamed)
/// * `buf` - Buffer to write the message to
///
/// # Errors
///
/// Currently infallible — always returns `Ok(())`. The `io::Result` return
/// type is preserved for forward compatibility.
///
/// # Panics
///
/// Panics via `msg_len` if `name.len()` exceeds `i32::MAX` bytes.
pub fn close(kind: u8, name: &str, buf: &mut BytesMut) -> io::Result<()> {
    buf.put_u8(b'C');
    buf.put_i32(4 + 1 + msg_len(name.len()) + 1);
    buf.put_u8(kind); // 'S' for statement, 'P' for portal
    buf.put_slice(name.as_bytes());
    buf.put_u8(0);
    Ok(())
}

/// Writes a terminate message.
///
/// Closes the connection gracefully. The server closes the connection
/// after receiving this message. No response is sent.
pub fn terminate(buf: &mut BytesMut) {
    buf.put_u8(b'X');
    buf.put_i32(4);
}

/// Writes a cancel request.
///
/// Cancels a running query. This is sent on a separate connection from
/// the main connection. The server does not send a response.
///
/// # Arguments
///
/// * `process_id` - Backend process ID (from `BackendKeyData` message)
/// * `secret_key` - Secret key (from `BackendKeyData` message)
/// * `buf` - Buffer to write the message to
pub fn cancel_request(process_id: i32, secret_key: i32, buf: &mut BytesMut) {
    buf.put_i32(16); // Length
    buf.put_i32(80877102); // Cancel request code
    buf.put_i32(process_id);
    buf.put_i32(secret_key);
}

/// Writes an `SSLRequest`.
///
/// Sent on a fresh TCP connection in place of the startup message. The
/// server answers with a single byte: `'S'` (proceed with a TLS handshake)
/// or `'N'` (TLS not available), after which the startup message follows,
/// encrypted or not.
pub fn ssl_request(buf: &mut BytesMut) {
    buf.put_i32(8); // Length
    buf.put_i32(80877103); // SSL request code
}

/// Writes a copy data message.
///
/// Sends a chunk of COPY data to the server during COPY IN operation.
/// The data format depends on the COPY format (text or binary).
///
/// # Arguments
///
/// * `data` - COPY data bytes
/// * `buf` - Buffer to write the message to
pub fn copy_data(data: &[u8], buf: &mut BytesMut) {
    buf.put_u8(b'd');
    buf.put_i32(4 + msg_len(data.len()));
    buf.put_slice(data);
}

/// Writes a copy done message.
///
/// Indicates that all COPY data has been sent (end of COPY IN operation).
/// The server responds with `CommandComplete`.
pub fn copy_done(buf: &mut BytesMut) {
    buf.put_u8(b'c');
    buf.put_i32(4);
}

/// Writes a copy fail message.
///
/// Aborts a COPY IN operation with an error message.
/// The server responds with `ErrorResponse`.
///
/// # Arguments
///
/// * `message` - Error message explaining why COPY failed
/// * `buf` - Buffer to write the message to
pub fn copy_fail(message: &str, buf: &mut BytesMut) {
    buf.put_u8(b'f');
    buf.put_i32(4 + msg_len(message.len()) + 1);
    buf.put_slice(message.as_bytes());
    buf.put_u8(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_request_bytes() {
        let mut buf = BytesMut::new();
        ssl_request(&mut buf);
        assert_eq!(&buf[..], &[0x00, 0x00, 0x00, 0x08, 0x04, 0xD2, 0x16, 0x2F]);
    }

    #[test]
    fn cancel_request_bytes() {
        let mut buf = BytesMut::new();
        cancel_request(7, -2, &mut buf);
        assert_eq!(
            &buf[..],
            &[
                0x00, 0x00, 0x00, 0x10, 0x04, 0xD2, 0x16, 0x2E, 0x00, 0x00, 0x00, 0x07, 0xFF, 0xFF,
                0xFF, 0xFE,
            ]
        );
    }
}
