// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Error types for the Hyper client.
//!
//! [`Error`] is a flat enum: one variant per failure mode, matched
//! directly, with no `kind()` discriminator and no `Box<dyn StdError>`
//! cause channel. That is the shape the [Microsoft Pragmatic Rust
//! Guidelines][msrg] call for (M-ERRORS-CANONICAL-STRUCTS,
//! M-ERRORS-AVOID-WRAPPING-AND-AS-DYN), and it mirrors the public
//! `hyperdb_api::Error` this type feeds.
//!
//! This type is **internal**. It is not re-exported from `hyperdb-api`;
//! callers of the public API match on `hyperdb_api::Error` instead, which
//! `From<client::Error>` produces.
//!
//! [msrg]: https://microsoft.github.io/rust-guidelines/

use std::io;

use thiserror::Error as ThisError;

/// The error type for Hyper client operations.
///
/// Variants that can carry a server-supplied SQLSTATE expose it as a
/// field, so callers match on it structurally rather than scraping the
/// message. The remaining variants are single-string: whatever context
/// exists is already rendered into that string by the constructor.
///
/// Deliberately **not** `#[non_exhaustive]`. That attribute buys
/// forward-compatibility for downstream matches, which this type has no
/// use for: it is internal, not re-exported, and `hyperdb-api` is its only
/// consumer and ships from this same workspace in lockstep. What it would
/// cost is the compile-time exhaustiveness check on
/// `From<client::Error> for hyperdb_api::Error` — the one place a new
/// variant must be given a public mapping. Better that adding a variant
/// breaks that build than silently degrades to `Error::Internal`.
#[derive(Debug, ThisError)]
pub enum Error {
    /// Connection failed.
    #[error("{message}")]
    Connection {
        /// Human-readable description of the failure.
        message: String,
        /// SQLSTATE, when the failure arrived from the server (gRPC
        /// reports connection-class SQLSTATEs in the `08xxx` family).
        sqlstate: Option<String>,
    },

    /// Authentication failed.
    #[error("{0}")]
    Authentication(String),

    /// Query execution failed.
    ///
    /// The one variant that carries the server's full diagnostic
    /// payload — `DETAIL` and `HINT` are surfaced verbatim by the public
    /// `hyperdb_api::Error::Server` variant this maps to.
    #[error("{message}{}", render_detail(message, detail.as_deref()))]
    Query {
        /// The primary error message.
        message: String,
        /// SQLSTATE code, when the server supplied one.
        sqlstate: Option<String>,
        /// The server's `DETAIL` field.
        detail: Option<String>,
        /// The server's `HINT` field.
        hint: Option<String>,
    },

    /// Invalid response from server.
    #[error("{0}")]
    Protocol(String),

    /// I/O error.
    #[error("{0}")]
    Io(String),

    /// Configuration error.
    #[error("{0}")]
    Config(String),

    /// TLS negotiation, handshake or certificate-verification failure.
    ///
    /// Includes a server that refuses TLS when the configured mode requires
    /// it. Unreadable certificate files and invalid option combinations are
    /// [`Error::Config`] instead.
    #[error("{0}")]
    Tls(String),

    /// Operation timed out.
    #[error("{0}")]
    Timeout(String),

    /// Operation was cancelled.
    #[error("{message}")]
    Cancelled {
        /// Human-readable description of the cancellation.
        message: String,
        /// SQLSTATE, typically `57014` (`query_canceled`).
        sqlstate: Option<String>,
    },

    /// The connection was closed.
    #[error("{message}")]
    Closed {
        /// Human-readable description.
        message: String,
        /// SQLSTATE, when the server supplied one.
        sqlstate: Option<String>,
    },

    /// Type conversion error.
    #[error("{0}")]
    Conversion(String),

    /// Feature not supported by this connection type.
    #[error("{0}")]
    FeatureNotSupported(String),

    /// Other error.
    #[error("{0}")]
    Other(String),
}

/// Renders the `": {detail}"` suffix for [`Error::Query`]'s `Display`,
/// suppressing it when `message` already contains the detail text.
///
/// gRPC's `decode_error_info` builds `"{primary}: {customer_detail}"` as
/// the message and *also* reports `customer_detail` separately, so without
/// this guard the detail would print twice.
fn render_detail(message: &str, detail: Option<&str>) -> String {
    match detail {
        Some(detail) if !message.contains(detail) => format!(": {detail}"),
        _ => String::new(),
    }
}

impl Error {
    // Constructors. Every variant has one taking `impl Into<String>`;
    // the variants with a SQLSTATE field default it to `None` here and
    // are built with struct literals where a code is available.

    /// Creates a connection error with no SQLSTATE.
    pub fn connection(message: impl Into<String>) -> Self {
        Error::Connection {
            message: message.into(),
            sqlstate: None,
        }
    }

    /// Creates an authentication error.
    pub fn authentication(message: impl Into<String>) -> Self {
        Error::Authentication(message.into())
    }

    /// Creates a query error with no SQLSTATE, detail, or hint.
    pub fn query(message: impl Into<String>) -> Self {
        Error::Query {
            message: message.into(),
            sqlstate: None,
            detail: None,
            hint: None,
        }
    }

    /// Creates a protocol error.
    pub fn protocol(message: impl Into<String>) -> Self {
        Error::Protocol(message.into())
    }

    /// Creates an I/O error from a message.
    ///
    /// Prefer [`Error::from_io`] when an [`io::Error`] is in hand.
    pub fn io(message: impl Into<String>) -> Self {
        Error::Io(message.into())
    }

    /// Creates a configuration error.
    pub fn config(message: impl Into<String>) -> Self {
        Error::Config(message.into())
    }

    /// Creates a TLS error.
    pub fn tls(message: impl Into<String>) -> Self {
        Error::Tls(message.into())
    }

    /// Creates a timeout error.
    pub fn timeout(message: impl Into<String>) -> Self {
        Error::Timeout(message.into())
    }

    /// Creates a cancellation error with no SQLSTATE.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Error::Cancelled {
            message: message.into(),
            sqlstate: None,
        }
    }

    /// Creates a closed-connection error with no SQLSTATE.
    pub fn closed(message: impl Into<String>) -> Self {
        Error::Closed {
            message: message.into(),
            sqlstate: None,
        }
    }

    /// Creates a type-conversion error.
    pub fn conversion(message: impl Into<String>) -> Self {
        Error::Conversion(message.into())
    }

    /// Creates a "feature not supported" error.
    ///
    /// Used when an operation is not available on a particular connection
    /// type (e.g. write operations on gRPC connections).
    pub fn feature_not_supported(message: impl Into<String>) -> Self {
        Error::FeatureNotSupported(message.into())
    }

    /// Creates a generic "other" error.
    pub fn other(message: impl Into<String>) -> Self {
        Error::Other(message.into())
    }

    // Convenience constructors for the common shapes.

    /// Creates an error from an [`io::Error`].
    ///
    /// rustls reports handshake failures and TLS alerts as an `io::Error`
    /// wrapping a `rustls::Error`; those become [`Error::Tls`]. Under TLS 1.3
    /// a server's rejection of the client certificate arrives only on the
    /// first read after the handshake, so the classification has to happen
    /// wherever I/O errors are converted, not only during negotiation.
    /// Everything else is [`Error::Io`].
    ///
    /// Takes the error by value so it can be used point-free as
    /// `.map_err(Error::from_io)`.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "call-site ergonomics: consumed as a `map_err` function reference"
    )]
    #[must_use]
    pub fn from_io(err: io::Error) -> Self {
        match err
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        {
            Some(tls_err) => Error::Tls(tls_err.to_string()),
            None => Error::Io(err.to_string()),
        }
    }

    /// Creates an error from a database error response.
    #[must_use]
    pub fn db(severity: &str, code: &str, message: &str) -> Self {
        Error::Query {
            message: format!("{severity}: {message} ({code})"),
            sqlstate: Some(code.to_string()),
            detail: None,
            hint: None,
        }
    }

    /// Returns the error message, without any `DETAIL` suffix that
    /// `Display` would append.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Error::Connection { message, .. }
            | Error::Query { message, .. }
            | Error::Cancelled { message, .. }
            | Error::Closed { message, .. } => message,
            Error::Authentication(message)
            | Error::Protocol(message)
            | Error::Io(message)
            | Error::Config(message)
            | Error::Tls(message)
            | Error::Timeout(message)
            | Error::Conversion(message)
            | Error::FeatureNotSupported(message)
            | Error::Other(message) => message,
        }
    }

    /// Returns the error detail, if available.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Error::Query { detail, .. } => detail.as_deref(),
            _ => None,
        }
    }

    /// Returns the error hint, if available.
    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        match self {
            Error::Query { hint, .. } => hint.as_deref(),
            _ => None,
        }
    }

    /// Extracts the `PostgreSQL` SQLSTATE code from the error, if present.
    ///
    /// SQLSTATE codes are 5-character codes that identify error conditions.
    /// See: <https://www.postgresql.org/docs/current/errcodes-appendix.html>
    ///
    /// For [`Error::Query`] with no stored code, falls back to scraping the
    /// trailing `(CODE)` that Hyper appends to wire error messages.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::Error;
    ///
    /// let err = Error::db("ERROR", "42P04", "database already exists");
    /// assert_eq!(err.sqlstate(), Some("42P04"));
    /// ```
    #[must_use]
    pub fn sqlstate(&self) -> Option<&str> {
        match self {
            Error::Connection { sqlstate, .. }
            | Error::Cancelled { sqlstate, .. }
            | Error::Closed { sqlstate, .. } => sqlstate.as_deref(),
            Error::Query {
                sqlstate, message, ..
            } => match sqlstate {
                Some(code) => Some(code),
                // Backwards compatibility: older paths encode the code in
                // the message rather than storing it.
                None => extract_sqlstate(message),
            },
            _ => None,
        }
    }
}

/// Extracts the SQLSTATE code from a Hyper error message.
///
/// Hyper error messages have the format: "SEVERITY: message (CODE)"
/// where CODE is the 5-character SQLSTATE code.
fn extract_sqlstate(message: &str) -> Option<&str> {
    // Find the last occurrence of '(' which should contain the SQLSTATE code
    let start = message.rfind('(')?;
    let end = message[start..].find(')')?;

    let code = message[start + 1..start + end].trim();

    // Validate that it looks like a SQLSTATE code (5 alphanumeric characters)
    if code.len() == 5 && code.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(code)
    } else {
        None
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::from_io(err)
    }
}

/// Result type for Hyper client operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlstate_extraction() {
        // Standard format: "SEVERITY: message (CODE)"
        let err = Error::db("ERROR", "42P04", "database \"test\" already exists");
        assert_eq!(err.sqlstate(), Some("42P04"));

        // Duplicate object
        let err = Error::db("ERROR", "42710", "duplicate object");
        assert_eq!(err.sqlstate(), Some("42710"));

        // Duplicate schema
        let err = Error::db("ERROR", "42P06", "schema \"public\" already exists");
        assert_eq!(err.sqlstate(), Some("42P06"));

        // Duplicate table
        let err = Error::db("ERROR", "42P07", "table \"users\" already exists");
        assert_eq!(err.sqlstate(), Some("42P07"));
    }

    #[test]
    fn test_sqlstate_non_query_error() {
        // Non-query errors carry no SQLSTATE unless one was supplied.
        let err = Error::connection("connection failed");
        assert_eq!(err.sqlstate(), None);

        let err = Error::timeout("operation timed out");
        assert_eq!(err.sqlstate(), None);
    }

    /// The gRPC path builds `Connection` / `Cancelled` / `Closed` with a
    /// server-supplied SQLSTATE; `sqlstate()` must surface it, because the
    /// public `hyperdb_api::Error` mapping forwards it to callers.
    #[test]
    fn test_sqlstate_on_non_query_variants() {
        let err = Error::Cancelled {
            message: "query canceled".to_string(),
            sqlstate: Some("57014".to_string()),
        };
        assert_eq!(err.sqlstate(), Some("57014"));

        let err = Error::Connection {
            message: "connection failure".to_string(),
            sqlstate: Some("08006".to_string()),
        };
        assert_eq!(err.sqlstate(), Some("08006"));

        let err = Error::Closed {
            message: "closed".to_string(),
            sqlstate: Some("08003".to_string()),
        };
        assert_eq!(err.sqlstate(), Some("08003"));
    }

    /// `Display` appends `DETAIL` only when the message doesn't already
    /// carry it — gRPC folds the detail into the message *and* reports it
    /// separately, and printing it twice was the bug this guard prevents.
    #[test]
    fn test_display_detail_suffix() {
        let err = Error::Query {
            message: "column not found".to_string(),
            sqlstate: None,
            detail: Some("column \"foo\" does not exist".to_string()),
            hint: None,
        };
        assert_eq!(
            err.to_string(),
            "column not found: column \"foo\" does not exist"
        );

        let err = Error::Query {
            message: "column not found: column \"foo\" does not exist".to_string(),
            sqlstate: None,
            detail: Some("column \"foo\" does not exist".to_string()),
            hint: None,
        };
        assert_eq!(
            err.to_string(),
            "column not found: column \"foo\" does not exist",
            "detail already present in the message must not be repeated"
        );
    }

    /// An `io::Error` renders exactly once. The previous struct-shaped
    /// error stored the same text as both `message` and `cause`, so
    /// `Display` emitted it twice.
    #[test]
    fn test_io_error_renders_once() {
        let io_err = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
        let err = Error::from(io_err);
        assert_eq!(err.to_string(), "refused");
    }

    #[test]
    fn test_extract_sqlstate_edge_cases() {
        // Valid SQLSTATE
        assert_eq!(extract_sqlstate("ERROR: message (42P04)"), Some("42P04"));

        // With spaces
        assert_eq!(extract_sqlstate("ERROR: message ( 42P04 )"), Some("42P04"));

        // Multiple parentheses (should extract last one)
        assert_eq!(
            extract_sqlstate("ERROR: (extra info) message (42P04)"),
            Some("42P04")
        );

        // Invalid: too short
        assert_eq!(extract_sqlstate("ERROR: message (42P)"), None);

        // Invalid: too long
        assert_eq!(extract_sqlstate("ERROR: message (42P044)"), None);

        // Invalid: non-alphanumeric
        assert_eq!(extract_sqlstate("ERROR: message (42-04)"), None);

        // No parentheses
        assert_eq!(extract_sqlstate("ERROR: message"), None);

        // Empty parentheses
        assert_eq!(extract_sqlstate("ERROR: message ()"), None);
    }

    /// rustls surfaces handshake failures and alerts as an `io::Error`
    /// wrapping a `rustls::Error`; those must classify as `Tls`, not `Io`.
    #[test]
    fn from_io_classifies_rustls_errors_as_tls() {
        let err = Error::from_io(io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::AlertReceived(rustls::AlertDescription::CertificateRequired),
        ));
        assert!(matches!(err, Error::Tls(_)), "got {err:?}");
        assert_eq!(err.message(), err.to_string());

        let err = Error::from_io(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
        assert!(
            matches!(err, Error::Io(ref msg) if msg == "reset"),
            "got {err:?}"
        );

        let err = Error::from(io::Error::new(io::ErrorKind::InvalidData, "not rustls"));
        assert!(matches!(err, Error::Io(_)), "got {err:?}");
    }
}
