// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! SQL escaping utilities.
//!
//! This module provides zero-cost wrapper types for safe SQL escaping.
//! Using the newtype pattern with [`std::fmt::Display`] ensures identifiers
//! and literals are properly escaped at format-time without extra allocations.
//!
//! # Why Newtype + Display?
//!
//! The alternative -- a function like `fn escape_identifier(s: &str) -> String`
//! -- allocates immediately even when the result is only used inside a larger
//! `format!()` call. The newtype pattern defers escaping to `Display::fmt`,
//! so the escaped output is written directly into the destination buffer.
//! This is the same approach used by `std::path::Path::display()`.
//!
//! The convenience functions [`escape_identifier`] and [`escape_literal`] are
//! provided for cases where a `String` is needed directly.

use std::fmt;

/// A wrapper that ensures a SQL identifier is properly escaped when formatted.
///
/// The identifier is **always** quoted, with embedded `"` doubled. Quoting
/// unconditionally is deliberate: the engine reserves keywords such as `order`,
/// `user` and `table`, and a bare reserved word is a syntax error (it broke
/// every `COPY` into such a table). Reserved words cannot be detected reliably
/// from the client, and for a plain lowercase name `"users"` and `users` are the
/// same relation, so the extra quotes never change meaning.
///
/// # Example
///
/// ```
/// use hyperdb_api_core::protocol::escape::SqlIdentifier;
///
/// assert_eq!(format!("{}", SqlIdentifier("users")), "\"users\"");
/// // Reserved words survive
/// assert_eq!(format!("{}", SqlIdentifier("order")), "\"order\"");
/// // Case is preserved
/// assert_eq!(format!("{}", SqlIdentifier("Segment")), "\"Segment\"");
/// // Internal quotes are escaped
/// assert_eq!(format!("{}", SqlIdentifier("my\"table")), "\"my\"\"table\"");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct SqlIdentifier<'a>(pub &'a str);

impl fmt::Display for SqlIdentifier<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&QuotedIdentifier(self.0), f)
    }
}

/// A wrapper that ensures a SQL string literal is properly escaped when formatted.
///
/// This wraps the string in single quotes and escapes any internal single quotes.
///
/// # Example
///
/// ```no_run
/// // Marked `no_run` to dodge a Windows Defender heuristic that intermittently
/// // refuses to launch this specific compiled doctest binary with
/// // `ERROR_ACCESS_DENIED`. The same assertions are exercised by
/// // `tests::test_sql_literal_display` so coverage is preserved.
/// use hyperdb_api_core::protocol::escape::SqlLiteral;
///
/// assert_eq!(format!("{}", SqlLiteral("hello")), "'hello'");
/// assert_eq!(format!("{}", SqlLiteral("it's")), "'it''s'");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct SqlLiteral<'a>(pub &'a str);

impl fmt::Display for SqlLiteral<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("'")?;
        for c in self.0.chars() {
            if c == '\'' {
                f.write_str("''")?;
            } else {
                write!(f, "{c}")?;
            }
        }
        f.write_str("'")
    }
}

/// Checks if a string is a valid unquoted identifier.
///
/// Valid unquoted identifiers:
/// - Start with a letter (a-z, A-Z) or underscore
/// - Contain only letters, digits (0-9), underscores, and dollar signs
/// - Are not SQL reserved words (this function doesn't check for reserved words)
#[must_use]
pub fn is_valid_unquoted_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }

    let mut chars = s.chars();

    // First character must be letter or underscore
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }

    // Rest can be letters, digits, underscores, or dollar signs
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Formats a qualified table name with proper escaping.
///
/// # Arguments
///
/// * `database` - Optional database name
/// * `schema` - Optional schema name
/// * `table` - Table name
///
/// # Example
///
/// ```
/// use hyperdb_api_core::protocol::escape::format_table_name;
///
/// assert_eq!(format_table_name(None, None, "users"), "\"users\"");
/// assert_eq!(format_table_name(None, Some("public"), "users"), "\"public\".\"users\"");
/// assert_eq!(
///     format_table_name(Some("mydb"), Some("public"), "users"),
///     "\"mydb\".\"public\".\"users\""
/// );
/// assert_eq!(format_table_name(None, None, "order"), "\"order\"");
/// ```
#[must_use]
pub fn format_table_name(database: Option<&str>, schema: Option<&str>, table: &str) -> String {
    match (database, schema) {
        (Some(db), Some(s)) => format!(
            "{}.{}.{}",
            SqlIdentifier(db),
            SqlIdentifier(s),
            SqlIdentifier(table)
        ),
        (None, Some(s)) => format!("{}.{}", SqlIdentifier(s), SqlIdentifier(table)),
        (Some(db), None) => format!("{}.{}", SqlIdentifier(db), SqlIdentifier(table)),
        (None, None) => format!("{}", SqlIdentifier(table)),
    }
}

// Backward compatibility functions - can be removed if not needed externally

/// Escapes a SQL identifier (table name, column name, etc.).
///
/// This is a convenience function that returns the escaped identifier as a String.
/// For more efficient formatting, use `SqlIdentifier` directly in format strings.
///
/// # Example
///
/// ```
/// use hyperdb_api_core::protocol::escape::escape_identifier;
///
/// assert_eq!(escape_identifier("table"), "\"table\"");
/// assert_eq!(escape_identifier("Segment"), "\"Segment\"");
/// ```
#[must_use]
pub fn escape_identifier(identifier: &str) -> String {
    format!("{}", SqlIdentifier(identifier))
}

/// A SQL identifier that is **always** quoted, whatever it contains.
///
/// Since [`SqlIdentifier`] now quotes unconditionally too, the two render
/// identically; this type remains as the explicit spelling of that intent.
/// `"users"` and `users` name the same relation, so the extra quotes never
/// change meaning, and reserved words such as `order` stay legal.
///
/// # Example
///
/// ```
/// use hyperdb_api_core::protocol::escape::QuotedIdentifier;
///
/// // Reserved words survive
/// assert_eq!(format!("{}", QuotedIdentifier("select")), "\"select\"");
/// assert_eq!(format!("{}", QuotedIdentifier("users")), "\"users\"");
/// // Internal quotes are doubled
/// assert_eq!(format!("{}", QuotedIdentifier("a\"b")), "\"a\"\"b\"");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct QuotedIdentifier<'a>(pub &'a str);

impl fmt::Display for QuotedIdentifier<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"")?;
        for c in self.0.chars() {
            if c == '"' {
                f.write_str("\"\"")?;
            } else {
                write!(f, "{c}")?;
            }
        }
        f.write_str("\"")
    }
}

/// Escapes a SQL string literal.
///
/// This is a convenience function that returns the escaped literal as a String.
/// For more efficient formatting, use `SqlLiteral` directly in format strings.
///
/// # Example
///
/// ```
/// use hyperdb_api_core::protocol::escape::escape_literal;
///
/// assert_eq!(escape_literal("hello"), "'hello'");
/// assert_eq!(escape_literal("it's"), "'it''s'");
/// ```
#[must_use]
pub fn escape_literal(literal: &str) -> String {
    format!("{}", SqlLiteral(literal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sql_identifier_display() {
        // Always quoted, including reserved words and plain lowercase names
        assert_eq!(format!("{}", SqlIdentifier("table")), "\"table\"");
        assert_eq!(format!("{}", SqlIdentifier("order")), "\"order\"");
        assert_eq!(format!("{}", SqlIdentifier("my_table")), "\"my_table\"");
        assert_eq!(format!("{}", SqlIdentifier("Segment")), "\"Segment\"");
        assert_eq!(format!("{}", SqlIdentifier("my-table")), "\"my-table\"");
        assert_eq!(format!("{}", SqlIdentifier("1table")), "\"1table\"");
        assert_eq!(format!("{}", SqlIdentifier("my\"table")), "\"my\"\"table\"");
        assert_eq!(format!("{}", SqlIdentifier("")), "\"\"");
    }

    #[test]
    fn test_sql_literal_display() {
        assert_eq!(format!("{}", SqlLiteral("hello")), "'hello'");
        assert_eq!(format!("{}", SqlLiteral("it's")), "'it''s'");
        assert_eq!(format!("{}", SqlLiteral("")), "''");
    }

    #[test]
    fn test_is_valid_unquoted_identifier() {
        assert!(is_valid_unquoted_identifier("table"));
        assert!(is_valid_unquoted_identifier("_private"));
        assert!(is_valid_unquoted_identifier("table1"));
        assert!(is_valid_unquoted_identifier("my$var"));

        assert!(!is_valid_unquoted_identifier(""));
        assert!(!is_valid_unquoted_identifier("1table"));
        assert!(!is_valid_unquoted_identifier("my-table"));
        assert!(!is_valid_unquoted_identifier("my table"));
    }

    #[test]
    fn test_format_table_name() {
        assert_eq!(format_table_name(None, None, "users"), "\"users\"");
        assert_eq!(format_table_name(None, None, "order"), "\"order\"");
        assert_eq!(
            format_table_name(None, Some("public"), "users"),
            "\"public\".\"users\""
        );
        assert_eq!(
            format_table_name(Some("mydb"), Some("public"), "users"),
            "\"mydb\".\"public\".\"users\""
        );
        assert_eq!(
            format_table_name(None, Some("my schema"), "users"),
            "\"my schema\".\"users\""
        );
    }

    #[test]
    fn test_sql_identifier_in_format() {
        let sql = format!(
            "SELECT {} FROM {}",
            SqlIdentifier("Customer ID"),
            SqlIdentifier("users")
        );
        assert_eq!(sql, "SELECT \"Customer ID\" FROM \"users\"");
    }

    // Backward compat function tests
    #[test]
    fn test_escape_identifier() {
        assert_eq!(escape_identifier("table"), "\"table\"");
        assert_eq!(escape_identifier("Segment"), "\"Segment\"");
    }

    #[test]
    fn test_escape_literal() {
        assert_eq!(escape_literal("hello"), "'hello'");
        assert_eq!(escape_literal("it's"), "'it''s'");
    }
}
