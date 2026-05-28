// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Error types for the pure Rust Hyper API.
//!
//! Callers match directly on [`Error`] variants. There is no `kind()`
//! indirection, no `Other` catch-all, and no `Box<dyn StdError>`
//! cause channel — see the [Microsoft Pragmatic Rust Guidelines][1]
//! M-ERRORS-CANONICAL-STRUCTS and M-ERRORS-AVOID-WRAPPING-AND-AS-DYN.
//!
//! Internal errors from [`hyperdb_api_core::client::Error`] are mapped
//! into this flat enum at the crate boundary via the `From` impl below.
//!
//! [1]: https://microsoft.github.io/rust-guidelines/

use thiserror::Error as ThisError;

/// The error type for Hyper API operations.
///
/// This enum is `#[non_exhaustive]`: new variants may be added in minor
/// releases, so match arms must include a wildcard `_ =>` pattern.
///
/// Struct variants (`Connection`, `Server`, `Column`,
/// `ColumnIndexOutOfBounds`, `Internal`) cannot use Rust's
/// `#[non_exhaustive]` (E0639), so forward-compatibility for new fields
/// relies on construction via the provided constructors:
///
/// - [`Self::internal`] for [`Self::Internal`]
/// - [`Self::connection`] / [`Self::connection_with_io`] for [`Self::Connection`]
/// - [`Self::server`] for [`Self::Server`]
/// - [`Self::column`] for [`Self::Column`]
/// - [`Self::column_index_out_of_bounds`] for [`Self::ColumnIndexOutOfBounds`]
///
/// Downstream code that uses struct-expression syntax for these
/// variants will fail to compile if a new field is added in a minor
/// release; using the constructors keeps callers source-compatible.
#[derive(Debug, ThisError)]
#[non_exhaustive]
pub enum Error {
    // ---- Connection / transport ----------------------------------------
    /// Connection-level failure (network, handshake, lifecycle, socket
    /// I/O). Carries the underlying [`std::io::Error`] when one is
    /// available; the type is erased at the wire-protocol boundary in
    /// `hyperdb-api-core`, so `source` is `None` for errors that
    /// originated there.
    ///
    /// Construct via [`Self::connection`] or [`Self::connection_with_io`].
    #[error("connection error: {message}")]
    Connection {
        /// Human-readable description.
        message: String,
        /// Underlying I/O error, if available.
        #[source]
        source: Option<std::io::Error>,
    },

    /// Authentication failed.
    #[error("authentication failed: {0}")]
    Authentication(String),

    /// TLS handshake or configuration failure.
    #[error("TLS error: {0}")]
    Tls(String),

    // ---- Server-side ---------------------------------------------------
    /// Server-side error (a SQL query or DDL command failed at the
    /// server). `sqlstate` is the 5-character `PostgreSQL` SQLSTATE
    /// code when the server reported one. `detail` and `hint` mirror
    /// the structured fields the server may include in its error
    /// response and are appended to the `Display` output when present.
    #[error(
        "server error{}: {message}{}{}",
        sqlstate.as_ref().map(|s| format!(" ({s})")).unwrap_or_default(),
        detail.as_ref().map(|d| format!("\nDETAIL: {d}")).unwrap_or_default(),
        hint.as_ref().map(|h| format!("\nHINT: {h}")).unwrap_or_default(),
    )]
    Server {
        /// The 5-character `PostgreSQL` SQLSTATE code, if reported.
        sqlstate: Option<String>,
        /// The primary error message from the server.
        message: String,
        /// Additional detail line from the server's error response.
        detail: Option<String>,
        /// Resolution hint from the server's error response.
        hint: Option<String>,
    },

    /// Wire-protocol or framing error.
    #[error("protocol error: {0}")]
    Protocol(String),

    // ---- I/O -----------------------------------------------------------
    /// Direct I/O error (file system, non-network sockets) at the SDK
    /// boundary. Network I/O during connection lifecycle is reported as
    /// [`Self::Connection`] instead.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    // ---- Lifecycle -----------------------------------------------------
    /// Operation attempted on a closed connection.
    #[error("connection closed: {0}")]
    Closed(String),

    /// Operation timed out.
    #[error("operation timed out: {0}")]
    Timeout(String),

    /// Operation was cancelled.
    #[error("operation cancelled: {0}")]
    Cancelled(String),

    // ---- Type / value --------------------------------------------------
    /// Type or value conversion failed (out-of-range numeric, malformed
    /// binary value, scalar query returned no rows, etc.). For
    /// column-specific decoding errors, prefer [`Self::Column`].
    #[error("conversion error: {0}")]
    Conversion(String),

    /// Configuration error (invalid endpoint, missing env var, bad
    /// option combination).
    #[error("configuration error: {0}")]
    Config(String),

    /// Feature is not supported on this connection or transport.
    #[error("feature not supported: {0}")]
    FeatureNotSupported(String),

    // ---- Catalog / validation ------------------------------------------
    /// Database identifier is invalid (empty, exceeds the `PostgreSQL`
    /// 63-byte limit, or violates other naming rules).
    #[error("invalid name: {0}")]
    InvalidName(String),

    /// Table definition is invalid (zero columns, conflicting
    /// attributes).
    #[error("invalid table definition: {0}")]
    InvalidTableDefinition(String),

    /// Database object (schema, table, etc.) was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Database object already exists.
    #[error("already exists: {0}")]
    AlreadyExists(String),

    // ---- Column / row mapping ------------------------------------------
    /// Structured error for named-column access in row decoding. Used
    /// by `FromRow` impls and `Row::try_get` / `Row::get_by_name` to
    /// signal which column failed and why.
    #[error("column {name}: {kind}")]
    Column {
        /// The column name.
        name: String,
        /// The structured cause of the column-access failure.
        #[source]
        kind: ColumnErrorKind,
    },

    /// Column index was out of bounds for the row. Used for positional
    /// access; named access uses [`Self::Column`] with
    /// [`ColumnErrorKind::Missing`].
    #[error("column index {idx} out of bounds (row has {column_count} columns)")]
    ColumnIndexOutOfBounds {
        /// The requested 0-based column index.
        idx: usize,
        /// The actual column count of the row.
        column_count: usize,
    },

    // ---- Internal ------------------------------------------------------
    /// Internal invariant violation. Used as a default for state
    /// assertions that should be unreachable in correct callers;
    /// callers generally cannot recover beyond logging and bailing.
    ///
    /// Construction of this variant should be rare — every site is a
    /// candidate for either a more specific variant or removal once
    /// the assertion is proven unreachable.
    ///
    /// Construct via [`Self::internal`].
    #[error("internal error: {message}")]
    Internal {
        /// Human-readable description of what invariant was violated.
        message: String,
    },
}

/// The structured cause of an [`Error::Column`].
#[derive(Debug, ThisError)]
#[non_exhaustive]
pub enum ColumnErrorKind {
    /// Column name was not found in the result schema.
    #[error("column not found")]
    Missing,

    /// Column was SQL `NULL` but the target type was not `Option<T>`.
    #[error("unexpected NULL")]
    Null,

    /// Column value could not be decoded as the target type.
    #[error("type mismatch: expected {expected}, got {actual}")]
    TypeMismatch {
        /// Rust type name the caller asked for.
        expected: String,
        /// Hyper SQL type name (or descriptive label) of the column.
        actual: String,
    },
}

impl Error {
    /// Constructs an [`Self::Internal`] error. Prefer this over
    /// struct-expression syntax to remain source-compatible if new
    /// fields are added in a minor release.
    pub fn internal(message: impl Into<String>) -> Self {
        Error::Internal {
            message: message.into(),
        }
    }

    /// Constructs an [`Self::Connection`] error with no underlying I/O
    /// source. Prefer this over struct-expression syntax to remain
    /// source-compatible if new fields are added in a minor release.
    pub fn connection(message: impl Into<String>) -> Self {
        Error::Connection {
            message: message.into(),
            source: None,
        }
    }

    /// Constructs an [`Self::Connection`] error wrapping an underlying
    /// [`std::io::Error`]. Prefer this over struct-expression syntax
    /// to remain source-compatible if new fields are added in a minor
    /// release.
    pub fn connection_with_io(message: impl Into<String>, source: std::io::Error) -> Self {
        Error::Connection {
            message: message.into(),
            source: Some(source),
        }
    }

    /// Constructs an [`Self::Server`] error. Prefer this over
    /// struct-expression syntax to remain source-compatible if new
    /// fields are added in a minor release.
    pub fn server(
        sqlstate: Option<String>,
        message: impl Into<String>,
        detail: Option<String>,
        hint: Option<String>,
    ) -> Self {
        Error::Server {
            sqlstate,
            message: message.into(),
            detail,
            hint,
        }
    }

    /// Constructs an [`Self::Column`] error. Prefer this over
    /// struct-expression syntax to remain source-compatible if new
    /// fields are added in a minor release.
    pub fn column(name: impl Into<String>, kind: ColumnErrorKind) -> Self {
        Error::Column {
            name: name.into(),
            kind,
        }
    }

    /// Constructs an [`Self::ColumnIndexOutOfBounds`] error. Prefer
    /// this over struct-expression syntax to remain source-compatible
    /// if new fields are added in a minor release.
    pub fn column_index_out_of_bounds(idx: usize, column_count: usize) -> Self {
        Error::ColumnIndexOutOfBounds { idx, column_count }
    }

    // ---- Tuple-variant constructors ------------------------------------
    //
    // These accept `impl Into<String>` so callers can pass either `&str`,
    // `String`, or `format!(...)` without the `.to_string()` / `.into()`
    // ceremony every direct construction would otherwise require.

    /// Constructs an [`Self::Authentication`] error.
    pub fn authentication(message: impl Into<String>) -> Self {
        Error::Authentication(message.into())
    }

    /// Constructs an [`Self::Tls`] error.
    pub fn tls(message: impl Into<String>) -> Self {
        Error::Tls(message.into())
    }

    /// Constructs an [`Self::Protocol`] error.
    pub fn protocol(message: impl Into<String>) -> Self {
        Error::Protocol(message.into())
    }

    /// Constructs an [`Self::Closed`] error.
    pub fn closed(message: impl Into<String>) -> Self {
        Error::Closed(message.into())
    }

    /// Constructs an [`Self::Timeout`] error.
    pub fn timeout(message: impl Into<String>) -> Self {
        Error::Timeout(message.into())
    }

    /// Constructs an [`Self::Cancelled`] error.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Error::Cancelled(message.into())
    }

    /// Constructs an [`Self::Conversion`] error.
    pub fn conversion(message: impl Into<String>) -> Self {
        Error::Conversion(message.into())
    }

    /// Constructs an [`Self::Config`] error.
    pub fn config(message: impl Into<String>) -> Self {
        Error::Config(message.into())
    }

    /// Constructs an [`Self::FeatureNotSupported`] error.
    pub fn feature_not_supported(message: impl Into<String>) -> Self {
        Error::FeatureNotSupported(message.into())
    }

    /// Constructs an [`Self::InvalidName`] error.
    pub fn invalid_name(message: impl Into<String>) -> Self {
        Error::InvalidName(message.into())
    }

    /// Constructs an [`Self::InvalidTableDefinition`] error.
    pub fn invalid_table_definition(message: impl Into<String>) -> Self {
        Error::InvalidTableDefinition(message.into())
    }

    /// Constructs an [`Self::NotFound`] error.
    pub fn not_found(message: impl Into<String>) -> Self {
        Error::NotFound(message.into())
    }

    /// Constructs an [`Self::AlreadyExists`] error.
    pub fn already_exists(message: impl Into<String>) -> Self {
        Error::AlreadyExists(message.into())
    }

    /// Returns the error message in human-readable form. Equivalent to
    /// `self.to_string()`.
    #[must_use]
    pub fn message(&self) -> String {
        self.to_string()
    }

    /// Returns the `PostgreSQL` SQLSTATE code if this is a
    /// [`Self::Server`] error that carries one, otherwise `None`.
    ///
    /// SQLSTATE codes are 5-character strings — see the [`PostgreSQL`
    /// errcodes appendix][1].
    ///
    /// [1]: https://www.postgresql.org/docs/current/errcodes-appendix.html
    #[must_use]
    pub fn sqlstate(&self) -> Option<&str> {
        match self {
            Error::Server { sqlstate, .. } => sqlstate.as_deref(),
            _ => None,
        }
    }
}

// Internal mapping: `client::Error` → public `Error`. The mapping is
// exhaustive over `client::ErrorKind` (verified to NOT be
// `#[non_exhaustive]`); adding a kind in `hyperdb-api-core` will break
// this build until the mapping is updated, which is intended.
//
// `chain = err.to_string()` walks the inner error's full Display chain
// (message + cause + detail). We use it for tuple variants whose
// `Display` is just `"<prefix>: {0}"`, where embedding the chain into
// the single string field gives the caller the full picture.
//
// For the `Server` variant we use the *un-chained* `message` and pass
// `detail`/`hint` separately; the `Server` `Display` impl re-appends
// "DETAIL: ..." and "HINT: ..." lines from those fields, so using
// `chain` would duplicate the detail text.
//
// SQLSTATE: `client::Error::sqlstate()` may return `Some` for non-Query
// kinds (e.g. SQLSTATE 57014 query_canceled comes back as Cancelled).
// The flat enum only carries `sqlstate` on `Server`, so SQLSTATE codes
// from non-Query kinds are folded into the message via `chain` rather
// than surfaced via `Error::sqlstate()`. Documented in MIGRATING-0.3.
impl From<hyperdb_api_core::client::Error> for Error {
    fn from(err: hyperdb_api_core::client::Error) -> Self {
        use hyperdb_api_core::client::ErrorKind as CoreKind;

        let chain = err.to_string();
        let kind = err.kind();
        let sqlstate = err.sqlstate().map(str::to_string);
        let detail = err.detail().map(str::to_string);
        let hint = err.hint().map(str::to_string);
        let message = err.message().to_string();

        match kind {
            CoreKind::Connection => Error::Connection {
                message: chain,
                source: None,
            },
            CoreKind::Authentication => Error::Authentication(chain),
            // Use unchained `message` here: detail/hint are passed as
            // separate fields and the `Server` Display impl re-renders
            // them. Using `chain` would duplicate detail text.
            CoreKind::Query => Error::Server {
                sqlstate,
                message,
                detail,
                hint,
            },
            CoreKind::Protocol => Error::Protocol(chain),
            // Wire-level I/O failures are reported as Connection errors
            // (the underlying io::Error is type-erased in core, so we
            // cannot recover it as a typed `source` here).
            CoreKind::Io => Error::Connection {
                message: chain,
                source: None,
            },
            CoreKind::Config => Error::Config(chain),
            CoreKind::Timeout => Error::Timeout(chain),
            CoreKind::Cancelled => Error::Cancelled(chain),
            CoreKind::Closed => Error::Closed(chain),
            CoreKind::Conversion => Error::Conversion(chain),
            CoreKind::FeatureNotSupported => Error::FeatureNotSupported(chain),
            CoreKind::Other => Error::Internal { message: chain },
        }
    }
}

// `Infallible` is the error type for identity `TryFrom`/`TryInto`
// conversions. Generic APIs that take `T: TryInto<U>` and bound
// `Error: From<T::Error>` (e.g. `TableDefinition::from_table_name`)
// require this impl to compile when callers pass a value that is
// already the target type. The body is unreachable because
// `Infallible` has no values.
impl From<std::convert::Infallible> for Error {
    fn from(_: std::convert::Infallible) -> Self {
        unreachable!("Infallible has no values")
    }
}

/// Result type for Hyper API operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use hyperdb_api_core::client::{Error as CoreError, ErrorKind as CoreKind};

    #[test]
    fn server_display_includes_sqlstate_detail_and_hint() {
        let err = Error::server(
            Some("23505".to_string()),
            "duplicate key value violates unique constraint",
            Some("Key (id)=(42) already exists.".to_string()),
            Some("Choose a different key.".to_string()),
        );
        let s = err.to_string();
        assert!(s.contains("server error (23505)"), "got: {s}");
        assert!(
            s.contains("duplicate key value violates unique constraint"),
            "got: {s}"
        );
        assert!(
            s.contains("\nDETAIL: Key (id)=(42) already exists."),
            "got: {s}"
        );
        assert!(s.contains("\nHINT: Choose a different key."), "got: {s}");
    }

    #[test]
    fn server_display_omits_missing_optional_fields() {
        let err = Error::server(None, "syntax error at end of input", None, None);
        let s = err.to_string();
        assert_eq!(s, "server error: syntax error at end of input");
    }

    #[test]
    fn from_client_error_query_does_not_duplicate_detail() {
        // Build a client::Error with detail; client::Error::Display
        // appends ": {detail}" inline. The flat-Error mapping must
        // not also add "\nDETAIL: {detail}" — that would duplicate the
        // text. We verify by counting occurrences.
        let core = CoreError::new_with_details(
            CoreKind::Query,
            "duplicate key value",
            Some("Key (id)=(42) already exists.".to_string()),
            Some("Choose a different key.".to_string()),
            Some("23505".to_string()),
        );
        let public: Error = core.into();
        let s = public.to_string();
        // The detail text should appear exactly once in the rendered
        // string. (Once on the DETAIL line; not also inline in message.)
        let count = s.matches("Key (id)=(42) already exists.").count();
        assert_eq!(count, 1, "detail must appear exactly once; got: {s}");
        let hint_count = s.matches("Choose a different key.").count();
        assert_eq!(hint_count, 1, "hint must appear exactly once; got: {s}");
        // Verify SQLSTATE is preserved.
        assert_eq!(public.sqlstate(), Some("23505"));
    }

    #[test]
    fn from_client_error_exhaustive_over_kinds() {
        // Smoke test: every ErrorKind maps cleanly with no panic.
        // (Compilation already enforces exhaustiveness.)
        for kind in [
            CoreKind::Connection,
            CoreKind::Authentication,
            CoreKind::Query,
            CoreKind::Protocol,
            CoreKind::Io,
            CoreKind::Config,
            CoreKind::Timeout,
            CoreKind::Cancelled,
            CoreKind::Closed,
            CoreKind::Conversion,
            CoreKind::FeatureNotSupported,
            CoreKind::Other,
        ] {
            let core = CoreError::new(kind, "test message");
            let public: Error = core.into();
            // Each variant's Display must include the message text.
            assert!(
                public.to_string().contains("test message"),
                "{kind:?} mapping lost the message: {public}",
            );
        }
    }

    #[test]
    fn sqlstate_returns_some_only_for_server() {
        let server = Error::server(Some("42P04".to_string()), "db exists", None, None);
        assert_eq!(server.sqlstate(), Some("42P04"));

        // Non-Server variants must return None even if the SQLSTATE
        // would have been present in the underlying client::Error.
        // Documented behavior: only Server-variant SQLSTATEs surface
        // through Error::sqlstate() in the flat enum.
        assert_eq!(Error::Conversion("...".into()).sqlstate(), None);
        assert_eq!(
            Error::Internal {
                message: "...".into()
            }
            .sqlstate(),
            None
        );
        assert_eq!(Error::Cancelled("...".into()).sqlstate(), None);
    }

    #[test]
    fn column_display_formats_name_and_kind() {
        let err = Error::column("user_id", ColumnErrorKind::Missing);
        assert_eq!(err.to_string(), "column user_id: column not found");

        let err = Error::column("score", ColumnErrorKind::Null);
        assert_eq!(err.to_string(), "column score: unexpected NULL");

        let err = Error::column(
            "count",
            ColumnErrorKind::TypeMismatch {
                expected: "i32".into(),
                actual: "TEXT".into(),
            },
        );
        assert_eq!(
            err.to_string(),
            "column count: type mismatch: expected i32, got TEXT"
        );
    }

    #[test]
    fn column_index_out_of_bounds_display() {
        let err = Error::column_index_out_of_bounds(5, 3);
        assert_eq!(
            err.to_string(),
            "column index 5 out of bounds (row has 3 columns)"
        );
    }

    #[test]
    fn connection_display_with_typed_io_source() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err = Error::connection_with_io("connecting to hyperd", io_err);
        let s = err.to_string();
        // Top-level message is the prefixed form.
        assert!(
            s.contains("connection error: connecting to hyperd"),
            "got: {s}"
        );
        // The typed source is recoverable via std::error::Error::source().
        use std::error::Error as StdError;
        let src = err.source().expect("connection_with_io must expose source");
        let io_src: &std::io::Error = src
            .downcast_ref::<std::io::Error>()
            .expect("source must downcast to io::Error");
        assert_eq!(io_src.kind(), std::io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn internal_constructor_round_trip() {
        let err = Error::internal("invariant violated");
        assert_eq!(err.to_string(), "internal error: invariant violated");
    }
}
