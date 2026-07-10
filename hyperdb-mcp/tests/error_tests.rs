// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the structured error types: construction, default suggestions,
//! JSON serialization format used in MCP tool responses, and the
//! `is_connection_lost` classifier that drives auto-reconnect.

use hyperdb_mcp::error::{is_connection_lost, ErrorCode, McpError};

/// Verify that constructing an `McpError` auto-populates a recovery suggestion
/// for error codes that have one (`HyperdNotFound` should suggest setting `HYPERD_PATH`).
#[test]
fn error_code_to_string() {
    let err = McpError::new(
        ErrorCode::HyperdNotFound,
        "hyperd not found at /usr/local/bin/hyperd",
    );
    assert_eq!(err.code, ErrorCode::HyperdNotFound);
    assert!(err.message.contains("hyperd not found"));
    assert!(err.suggestion.is_some());
}

/// Verify that `McpError` serializes to JSON with `SCREAMING_SNAKE_CASE` error codes
/// so LLM clients can pattern-match on the code field.
#[test]
fn error_to_json() {
    let err = McpError::new(ErrorCode::SqlError, "syntax error at position 42");
    let json = serde_json::to_value(&err).unwrap();
    assert_eq!(json["code"], "SQL_ERROR");
    assert_eq!(json["message"], "syntax error at position 42");
}

/// Every transport-disappearance variant the `hyper-client` layer can
/// emit must be recognized as connection-lost so `with_engine` tears
/// down the engine and reconnects on the next call.
#[test]
fn classifies_transport_disappearance_as_connection_lost() {
    // Exact strings drawn from the std / tokio / PG-wire error messages
    // `hyper-client` forwards when `hyperd` crashes or is killed.
    for msg in [
        "Broken pipe (os error 32)",
        "Connection reset by peer",
        "Connection refused (os error 61)",
        "connection closed by server",
        "unexpected EOF while reading frame header",
        "unexpected end of file during read",
        "server unexpectedly closed the connection",
        "socket is not connected",
    ] {
        assert!(
            is_connection_lost(msg),
            "expected transport error to be classified as connection-lost: {msg:?}",
        );
    }
}

/// Wire-protocol desync is a distinct failure mode from transport
/// disappearance: the socket is still open but `hyper-client` has
/// marked the connection desynchronized because a bounded drain
/// exhausted its budget. Subsequent operations fast-fail with a
/// "desynchronized" message, and the mcp server must treat that the
/// same way as a transport-lost error — recycle the engine, reconnect
/// on the next call.
///
/// Regression guard: this was the first concrete mcp-side gap discovered
/// after the `desynchronized` flag landed in `hyper-client`; without
/// this match, drain-poisoned connections would stay poisoned until the
/// process was restarted.
#[test]
fn classifies_desynchronized_wire_as_connection_lost() {
    // The exact message produced by `RawConnection::ensure_healthy` /
    // `AsyncRawConnection::ensure_healthy` in the `hyper-client` crate.
    let desync_msg = "connection is desynchronized from the server and \
                      cannot be reused; discard it and open a new one";
    assert!(
        is_connection_lost(desync_msg),
        "desynchronized wire must be classified as connection-lost",
    );

    // The match is by keyword, not the exact phrase, so variations
    // (different severity prefix, different punctuation) still fire.
    assert!(is_connection_lost(
        "ERROR: connection has been desynchronized and cannot continue",
    ));
    assert!(is_connection_lost("DESYNCHRONIZED: drop this conn"));
}

/// Unrelated errors must NOT be classified as connection-lost —
/// otherwise a syntax error would spuriously recycle the engine.
#[test]
fn does_not_classify_unrelated_errors_as_connection_lost() {
    for msg in [
        "syntax error at or near \"SELECT\"",
        "table \"foo\" does not exist",
        "permission denied for file /tmp/data.csv",
        "No such file or directory",
        "disk full",
    ] {
        assert!(
            !is_connection_lost(msg),
            "unrelated error must NOT be classified as connection-lost: {msg:?}",
        );
    }
}

/// SQLSTATE 22003 (`numeric_value_out_of_range`) is the exact error we now see
/// when a CSV value overflows its inferred column type. It must be surfaced
/// as `SchemaMismatch` with a suggestion that points the caller at the
/// partial-override workflow — an opaque "`InternalError`" here forces humans
/// to read wire-level messages and was the reason we built `inspect_file`.
#[test]
fn maps_22003_to_schema_mismatch_with_override_suggestion() {
    let upstream =
        hyperdb_api::Error::server(Some("22003".to_string()), "numeric overflow", None, None);
    let mcp: McpError = upstream.into();
    assert_eq!(mcp.code, ErrorCode::SchemaMismatch);
    let suggestion = mcp.suggestion.expect("22003 must have a suggestion");
    let lower = suggestion.to_lowercase();
    assert!(
        lower.contains("schema") && lower.contains("override"),
        "suggestion should mention schema override; got: {suggestion}",
    );
    assert!(
        lower.contains("bigint") || lower.contains("numeric"),
        "suggestion should hint at BIGINT/NUMERIC widenings; got: {suggestion}",
    );
}

/// The classifier must also fire on human-readable spellings, not just the
/// raw SQLSTATE code, because different hyperd versions format the message
/// differently.
#[test]
fn maps_out_of_range_phrase_to_schema_mismatch() {
    let upstream =
        hyperdb_api::Error::server(None, "value out of range for type integer", None, None);
    let mcp: McpError = upstream.into();
    assert_eq!(mcp.code, ErrorCode::SchemaMismatch);
    assert!(mcp.suggestion.is_some());
}

/// SQLSTATE 22P02 (`invalid_text_representation`) fires when a value can't be
/// parsed into its declared type — usually a text value in a DATE column.
/// The suggestion should steer callers toward overriding the column as TEXT
/// and casting in SQL rather than guessing a new type.
#[test]
fn maps_22p02_to_schema_mismatch_with_text_suggestion() {
    let upstream = hyperdb_api::Error::server(
        Some("22P02".to_string()),
        "invalid input syntax for type date",
        None,
        None,
    );
    let mcp: McpError = upstream.into();
    assert_eq!(mcp.code, ErrorCode::SchemaMismatch);
    let suggestion = mcp.suggestion.expect("22P02 must have a suggestion");
    assert!(
        suggestion.to_uppercase().contains("TEXT"),
        "suggestion should recommend overriding to TEXT; got: {suggestion}",
    );
}

/// The `ConnectionLost` suggestion message must mention both failure
/// modes the classifier now covers — crashed hyperd and wire desync —
/// so LLM callers and humans reading logs know what they're seeing.
#[test]
fn connection_lost_suggestion_mentions_both_triggers() {
    let err = McpError::new(ErrorCode::ConnectionLost, "connection is desynchronized");
    let suggestion = err.suggestion.expect("ConnectionLost has a suggestion");
    let lower = suggestion.to_lowercase();
    assert!(
        lower.contains("lost") || lower.contains("desync") || lower.contains("sync"),
        "suggestion should hint at both triggers; got: {suggestion}",
    );
    assert!(
        lower.contains("retry") || lower.contains("reconnect") || lower.contains("automatically"),
        "suggestion should tell the caller the server will recover; got: {suggestion}",
    );
}

/// A caller-supplied bad identifier — the shape the KV tools hit when a
/// `store`/`key` has a disallowed byte or exceeds the 512-byte limit —
/// arrives as `hyperdb_api::Error::InvalidName`. It must map to
/// `InvalidArgument`, not `InternalError`: the caller supplied something
/// wrong and can fix it, and the message already names the offending byte
/// or the length. Before this mapping the variant fell through to the
/// catch-all `InternalError` arm, mislabeling a validation failure as a
/// server-side bug (caught during the KV MCP smoke run).
#[test]
fn maps_invalid_name_to_invalid_argument() {
    for upstream in [
        // The exact messages `validate_kv_name` produces for the KV tools.
        hyperdb_api::Error::invalid_name(
            "KV key contains an invalid byte 0x20; allowed: A-Z a-z 0-9 _ . -",
        ),
        hyperdb_api::Error::invalid_name("KV store name must not be empty"),
        hyperdb_api::Error::invalid_name("KV key exceeds 512-byte limit (630 bytes)"),
    ] {
        let original = upstream.to_string();
        let mcp: McpError = upstream.into();
        assert_eq!(
            mcp.code,
            ErrorCode::InvalidArgument,
            "invalid name must be InvalidArgument, not InternalError: {original}",
        );
        // The message must survive so the caller learns what to fix.
        assert!(
            mcp.message.contains("invalid name"),
            "message should carry the validation detail; got: {}",
            mcp.message,
        );
        assert!(
            mcp.suggestion.is_some(),
            "InvalidArgument carries a self-correction suggestion",
        );
    }
}

/// `InvalidTableDefinition` shares the `InvalidName` reasoning — a
/// malformed table definition is something the caller (the LLM's tool
/// arguments) supplied and can correct — so it maps to `InvalidArgument`,
/// not the opaque `InternalError` the catch-all arm used to assign.
#[test]
fn maps_invalid_table_definition_to_invalid_argument() {
    let def: McpError =
        hyperdb_api::Error::invalid_table_definition("table must have at least one column").into();
    assert_eq!(def.code, ErrorCode::InvalidArgument);
}

/// `InvalidOperation` is deliberately NOT mapped to `InvalidArgument`. It
/// is hyperdb-api "caller-API misuse" where the caller is this MCP's own
/// Rust code (e.g. an inserter used out of sequence), not the LLM — an
/// occurrence signals an MCP bug the model cannot fix by changing tool
/// arguments, so it stays `InternalError`. This guards against a naive
/// "group it with the other caller-fixable variants" regression that
/// would hand the model a misleading "check your arguments" suggestion.
#[test]
fn keeps_invalid_operation_as_internal_error() {
    let op: McpError = hyperdb_api::Error::invalid_operation("inserter already finalized").into();
    assert_eq!(op.code, ErrorCode::InternalError);
}
