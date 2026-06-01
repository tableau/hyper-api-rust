// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! SQLSTATE-based Hyper error classification.
//!
//! Phase 0 spike S5 confirmed that Hyper returns PostgreSQL SQLSTATE codes as a
//! structured field on `Error::Server`. We branch on the **stable code**, not
//! fragile message text, so Hyper message wording changes don't break us.
//!
//! Relevant codes:
//! - `42P01` — undefined_table  → extract the table name and seed-and-retry
//! - `42703` — undefined_column → report the column name verbatim
//! - `42601` — syntax_error     → forward the message verbatim
//! - anything else              → forward as `HyperError`

use hyperdb_api::Error;

/// Classification of a Hyper error for compile-time validation purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorClass {
    /// SQLSTATE `42P01`: a table is missing. Contains the table name extracted
    /// from the error message.
    MissingTable(String),
    /// SQLSTATE `42703`: a column is missing. Contains the identifier verbatim.
    MissingColumn(String),
    /// SQLSTATE `42601`: SQL syntax error. The full message is forwarded.
    SyntaxError(String),
    /// Any other error (e.g. connection failure, internal error).
    Other(String),
}

/// Classify a `hyperdb_api::Error` by its SQLSTATE code.
pub fn classify(err: &Error) -> ErrorClass {
    match err.sqlstate() {
        Some("42P01") => {
            ErrorClass::MissingTable(extract_quoted_identifier(&format!("{err}"), "table"))
        }
        Some("42703") => {
            ErrorClass::MissingColumn(extract_quoted_identifier(&format!("{err}"), "column"))
        }
        Some("42601") => ErrorClass::SyntaxError(format!("{err}")),
        _ => ErrorClass::Other(format!("{err}")),
    }
}

/// Extract a double-quoted or single-quoted identifier from a Hyper error
/// message, with a fallback to the full message.
///
/// Phase 0 output for 42P01: `ERROR: table "ghosts" does not exist (42P01)`
/// Phase 0 output for 42703: `ERROR: unknown column 'ema1l' (42703)`
fn extract_quoted_identifier(message: &str, _kind: &str) -> String {
    // Try double-quoted first ("ghosts"), then single-quoted ('ema1l').
    if let Some(name) = extract_between(message, '"', '"') {
        return name;
    }
    if let Some(name) = extract_between(message, '\'', '\'') {
        return name;
    }
    // Fallback: return the whole message so we never lose information.
    message.to_owned()
}

fn extract_between(s: &str, open: char, close: char) -> Option<String> {
    let start = s.find(open)? + open.len_utf8();
    let end = s[start..].find(close)? + start;
    Some(s[start..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_double_quoted() {
        let msg = r#"ERROR: table "ghosts" does not exist (42P01)"#;
        assert_eq!(extract_quoted_identifier(msg, "table"), "ghosts");
    }

    #[test]
    fn extract_single_quoted() {
        let msg = "ERROR: unknown column 'ema1l' (42703)";
        assert_eq!(extract_quoted_identifier(msg, "column"), "ema1l");
    }

    #[test]
    fn extract_falls_back_to_full_message() {
        let msg = "ERROR: something unquoted happened";
        let result = extract_quoted_identifier(msg, "table");
        assert_eq!(result, msg);
    }
}
