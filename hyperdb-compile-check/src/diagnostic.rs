// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ValidationError` variants and human-readable formatting.
//!
//! All types here use plain Rust strings — no `syn`/`proc-macro2` token types.
//! The proc-macro shell converts these into `compile_error!` token streams.

/// The result of validating one `query_as!` / `query_scalar!` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// The struct used in `query_as!` has not been registered via
    /// `#[derive(Table)] #[hyperdb(register)]`.
    StructNotRegistered {
        /// The Rust struct ident string.
        struct_name: String,
    },

    /// One or more tables referenced by the SQL are not in the registry.
    TablesNotRegistered {
        /// Table names that were referenced but not registered.
        tables: Vec<String>,
    },

    /// The query's result schema is missing columns that the target struct
    /// requires (one entry per missing column name).
    MissingColumns {
        /// The Rust struct ident string.
        struct_name: String,
        /// Column names present in the struct but absent from the query result.
        missing: Vec<String>,
    },

    /// The SQL references a column that does not exist on any table in the
    /// query (SQLSTATE 42703). Distinct from `MissingColumns`, which is when
    /// the query is valid but omits a column the struct needs.
    UnknownColumn {
        /// The column identifier Hyper reported as undefined.
        column: String,
    },

    /// The SQL has a syntax error; the message is forwarded verbatim from Hyper.
    SqlSyntaxError {
        /// Hyper's error message.
        message: String,
    },

    /// An unexpected Hyper error occurred during dry-run.
    HyperError {
        /// Hyper's error message.
        message: String,
    },
}

impl ValidationError {
    /// Human-readable diagnostic message suitable for embedding in
    /// `compile_error!("...")`.
    pub fn to_diagnostic(&self) -> String {
        match self {
            Self::StructNotRegistered { struct_name } => format!(
                "type `{struct_name}` must `#[derive(Table)]` with `#[hyperdb(register)]` \
                 to be used with `query_as!`"
            ),
            Self::TablesNotRegistered { tables } => {
                if tables.len() == 1 {
                    format!(
                        "table {:?} is not registered; did you forget \
                         `#[derive(Table)] #[hyperdb(register)]` on the struct that maps to it?",
                        tables[0]
                    )
                } else {
                    format!(
                        "tables {tables:?} are not registered; add \
                         `#[derive(Table)] #[hyperdb(register)]` to the structs that map to them"
                    )
                }
            }
            Self::MissingColumns {
                struct_name,
                missing,
            } => {
                if missing.len() == 1 {
                    format!(
                        "`{struct_name}` requires column {:?} but the query does not project it; \
                         add it to the SELECT list or remove the field from `{struct_name}`",
                        missing[0]
                    )
                } else {
                    format!(
                        "`{struct_name}` requires columns {missing:?} but the query does not \
                         project them; add them to the SELECT list or remove the fields from \
                         `{struct_name}`"
                    )
                }
            }
            Self::UnknownColumn { column } => format!(
                "column {column:?} does not exist on any table in the query; \
                 check for a typo or a renamed/dropped column"
            ),
            Self::SqlSyntaxError { message } => {
                format!("SQL syntax error: {message}")
            }
            Self::HyperError { message } => {
                format!("Hyper validation error: {message}")
            }
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_diagnostic())
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_not_registered_message() {
        let e = ValidationError::StructNotRegistered {
            struct_name: "User".into(),
        };
        assert!(e.to_diagnostic().contains("User"));
        assert!(e.to_diagnostic().contains("derive(Table)"));
    }

    #[test]
    fn single_missing_column_message() {
        let e = ValidationError::MissingColumns {
            struct_name: "User".into(),
            missing: vec!["email".into()],
        };
        let msg = e.to_diagnostic();
        assert!(msg.contains("email"), "message: {msg}");
        assert!(msg.contains("User"), "message: {msg}");
    }

    #[test]
    fn multi_missing_columns_message() {
        let e = ValidationError::MissingColumns {
            struct_name: "User".into(),
            missing: vec!["email".into(), "name".into()],
        };
        let msg = e.to_diagnostic();
        assert!(msg.contains("email"), "message: {msg}");
        assert!(msg.contains("name"), "message: {msg}");
    }

    #[test]
    fn single_table_not_registered_message() {
        let e = ValidationError::TablesNotRegistered {
            tables: vec!["ghosts".into()],
        };
        assert!(e.to_diagnostic().contains("ghosts"));
        assert!(e.to_diagnostic().contains("derive(Table)"));
    }

    #[test]
    fn unknown_column_message() {
        let e = ValidationError::UnknownColumn { column: "d".into() };
        let msg = e.to_diagnostic();
        assert!(msg.contains("\"d\""), "message: {msg}");
        assert!(msg.contains("does not exist"), "message: {msg}");
        assert!(msg.contains("typo"), "message: {msg}");
    }
}
