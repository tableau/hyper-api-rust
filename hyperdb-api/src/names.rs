// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! SQL name types for safe identifier handling.
//!
//! This module provides types that properly escape SQL identifiers to prevent
//! SQL injection attacks.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use smallvec::SmallVec;

use crate::error::{Error, Result};

/// `PostgreSQL` identifier length limit (in characters).
pub(crate) const PG_IDENTIFIER_LIMIT: usize = 63;

/// Escapes a SQL identifier for safe use in queries.
///
/// This function properly quotes and escapes a name to prevent SQL injection.
/// The result is wrapped in double quotes with internal quotes escaped.
///
/// # Errors
///
/// Returns an error if the name exceeds the `PostgreSQL` identifier limit (63 characters).
/// This behavior is consistent with [`Name::try_new()`].
///
/// # Example
///
/// ```
/// use hyperdb_api::{escape_name, Result};
///
/// fn demo() -> Result<()> {
///     let escaped = escape_name("my_table")?;
///     assert_eq!(escaped, "\"my_table\"");
///
///     let special = escape_name("table\"with\"quotes")?;
///     assert_eq!(special, "\"table\"\"with\"\"quotes\"");
///     Ok(())
/// }
///
/// // Names exceeding 63 characters are rejected
/// let long_name = "a".repeat(64);
/// assert!(escape_name(&long_name).is_err());
/// ```
pub fn escape_name(name: &str) -> Result<String> {
    let len = name.chars().count();
    if len > PG_IDENTIFIER_LIMIT {
        return Err(Error::invalid_name(format!(
            "Name exceeds PostgreSQL identifier limit ({len} > {PG_IDENTIFIER_LIMIT})"
        )));
    }

    let escaped_inner = name.replace('"', "\"\"");
    Ok(format!("\"{escaped_inner}\""))
}

/// Escapes a database file path for safe use in SQL statements.
///
/// This function wraps the path in double quotes and escapes internal quotes,
/// just like [`escape_name()`], but **without** the 63-character `PostgreSQL`
/// identifier length limit. Use this for `CREATE DATABASE`, `ATTACH DATABASE`,
/// `COPY DATABASE`, and similar statements where the argument is a file path
/// rather than an SQL identifier.
///
/// # Example
///
/// ```
/// use hyperdb_api::escape_sql_path;
///
/// let simple = escape_sql_path("/tmp/data.hyper");
/// assert_eq!(simple, "\"/tmp/data.hyper\"");
///
/// let special = escape_sql_path("/tmp/my \"db\".hyper");
/// assert_eq!(special, "\"/tmp/my \"\"db\"\".hyper\"");
/// ```
#[must_use]
pub fn escape_sql_path(path: &str) -> String {
    let escaped_inner = path.replace('"', "\"\"");
    format!("\"{escaped_inner}\"")
}

/// Escapes a SQL string literal for safe use in queries.
///
/// This function properly quotes and escapes a string value to prevent SQL injection.
/// The result is wrapped in single quotes with internal quotes escaped.
///
/// # Example
///
/// ```
/// use hyperdb_api::escape_string_literal;
///
/// let escaped = escape_string_literal("hello");
/// assert_eq!(escaped, "'hello'");
///
/// let special = escape_string_literal("it's a test");
/// assert_eq!(special, "'it''s a test'");
/// ```
#[must_use]
pub fn escape_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Represents an escaped SQL identifier name.
///
/// `Name` stores both the properly quoted/escaped version (safe for SQL) and
/// the original unescaped version (for display/logging).
///
/// # Example
///
/// ```
/// use hyperdb_api::Name;
///
/// let name = Name::try_new("users")?;
/// assert_eq!(name.to_string(), "\"users\"");
/// assert_eq!(name.unescaped(), "users");
/// # Ok::<(), hyperdb_api::Error>(())
/// ```
#[derive(Clone, Debug)]
#[must_use = "Name represents a validated SQL identifier that should not be discarded. Use it in your SQL queries or table definitions"]
pub struct Name {
    /// The escaped name (safe for SQL).
    escaped: String,
    /// The original unescaped name.
    unescaped: String,
}

impl Name {
    /// Creates a new escaped SQL name.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is empty or exceeds the `PostgreSQL` identifier limit (63 characters).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::Name;
    ///
    /// let name = Name::try_new("users")?;
    /// assert_eq!(name.unescaped(), "users");
    /// # Ok::<(), hyperdb_api::Error>(())
    /// ```
    pub fn try_new(name: impl Into<String>) -> Result<Self> {
        let unescaped = name.into();
        if unescaped.is_empty() {
            return Err(Error::invalid_name("Name must not be empty"));
        }
        // escape_name validates the length limit and returns an error if exceeded
        let escaped = escape_name(&unescaped)?;
        Ok(Name { escaped, unescaped })
    }

    /// Returns the properly quoted and escaped string representation.
    ///
    /// This is safe to use directly in SQL queries.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.escaped
    }

    /// Returns the original unescaped name.
    ///
    /// **Warning:** Do not use this in SQL queries as it may be vulnerable to
    /// SQL injection. Use this only for logging or display purposes.
    #[must_use]
    pub fn unescaped(&self) -> &str {
        &self.unescaped
    }
}

/// Parses a dot-separated SQL identifier into parts, handling quoted sections.
///
/// This is a common parsing function used by Name, `SchemaName`, and `TableName`.
/// Uses `SmallVec` for efficiency since most identifiers have 1-3 parts.
fn parse_qualified_identifier(s: &str) -> SmallVec<[String; 3]> {
    let mut parts = SmallVec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // Toggle the "in_quotes" state
            '"' => {
                // Handle escaped quotes (double double-quotes)
                if in_quotes && chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next(); // skip the second quote
                } else {
                    in_quotes = !in_quotes;
                    // Don't add the quote character itself to current
                }
            }
            // Split on dots, but ONLY if we aren't inside quotes
            '.' if !in_quotes => {
                if !current.is_empty() {
                    parts.push(current.split_off(0));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.escaped)
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Self) -> bool {
        self.unescaped == other.unescaped
    }
}

impl Eq for Name {}

impl PartialOrd for Name {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Name {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.unescaped.cmp(&other.unescaped)
    }
}

impl Hash for Name {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.unescaped.hash(state);
    }
}

impl TryFrom<&str> for Name {
    type Error = Error;

    fn try_from(s: &str) -> Result<Self> {
        Self::try_new(s)
    }
}

impl TryFrom<&String> for Name {
    type Error = Error;

    fn try_from(s: &String) -> Result<Self> {
        Self::try_new(s.as_str())
    }
}

impl TryFrom<String> for Name {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        Self::try_new(s)
    }
}

impl FromStr for Name {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::try_new(s)
    }
}

/// Represents an escaped SQL database name.
///
/// # Example
///
/// ```
/// use hyperdb_api::{DatabaseName, Result};
///
/// # fn main() -> Result<()> {
/// let db = DatabaseName::try_new("mydb")?;
/// assert_eq!(db.to_string(), "\"mydb\"");
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[must_use = "DatabaseName represents a validated database identifier that should not be discarded. Use it in your connection or table definitions"]
pub struct DatabaseName {
    name: Name,
}

impl DatabaseName {
    /// Creates a new database name.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is empty or exceeds the `PostgreSQL` identifier limit.
    pub fn try_new(name: impl Into<String>) -> Result<Self> {
        Ok(DatabaseName {
            name: Name::try_new(name)?,
        })
    }

    /// Returns the name component.
    pub fn name(&self) -> &Name {
        &self.name
    }

    /// Returns the unescaped name.
    #[must_use]
    pub fn unescaped(&self) -> &str {
        self.name.unescaped()
    }
}

impl fmt::Display for DatabaseName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl TryFrom<&str> for DatabaseName {
    type Error = Error;

    fn try_from(s: &str) -> Result<Self> {
        Self::try_new(s)
    }
}

impl TryFrom<&String> for DatabaseName {
    type Error = Error;

    fn try_from(s: &String) -> Result<Self> {
        Self::try_new(s.as_str())
    }
}

impl TryFrom<String> for DatabaseName {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        Self::try_new(s)
    }
}

impl From<Name> for DatabaseName {
    fn from(name: Name) -> Self {
        DatabaseName { name }
    }
}

impl FromStr for DatabaseName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::try_new(s)
    }
}

/// Represents an escaped SQL schema name with optional database qualifier.
///
/// Uses the fluent builder pattern for constructing qualified schema names.
///
/// # Example
///
/// ```
/// use hyperdb_api::{SchemaName, Result};
///
/// # fn main() -> Result<()> {
/// // Simple schema name
/// let schema = SchemaName::try_new("public")?;
/// assert_eq!(schema.to_string(), "\"public\"");
///
/// // Qualified schema name using fluent builder
/// let qualified = SchemaName::try_new("public")?.with_database("mydb")?;
/// assert_eq!(qualified.to_string(), "\"mydb\".\"public\"");
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[must_use = "SchemaName represents a validated schema identifier that should not be discarded. Use it in your table definitions or queries"]
pub struct SchemaName {
    database: Option<DatabaseName>,
    schema: Name,
}

impl SchemaName {
    /// Creates a new schema name without a database qualifier (the starting point).
    ///
    /// # Errors
    ///
    /// Returns an error if the schema name is empty or exceeds the `PostgreSQL` identifier limit.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::SchemaName;
    ///
    /// let schema = SchemaName::try_new("public")?;
    /// assert_eq!(schema.to_string(), "\"public\"");
    /// # Ok::<(), hyperdb_api::Error>(())
    /// ```
    pub fn try_new(schema: impl Into<String>) -> Result<Self> {
        Ok(SchemaName {
            database: None,
            schema: Name::try_new(schema)?,
        })
    }

    /// Builder method: Sets the database qualifier.
    ///
    /// This method is part of the fluent builder pattern and can be chained.
    /// Returns `Result<Self>` to allow fallible method chaining.
    ///
    /// # Errors
    ///
    /// Returns an error if the database name is empty or exceeds the `PostgreSQL` identifier limit.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::SchemaName;
    ///
    /// let schema = SchemaName::try_new("public")?.with_database("mydb")?;
    /// assert_eq!(schema.to_string(), "\"mydb\".\"public\"");
    /// # Ok::<(), hyperdb_api::Error>(())
    /// ```
    pub fn with_database(mut self, database: impl Into<String>) -> Result<Self> {
        self.database = Some(DatabaseName::try_new(database)?);
        Ok(self)
    }

    /// Returns the database name, if any.
    #[must_use]
    pub fn database(&self) -> Option<&DatabaseName> {
        self.database.as_ref()
    }

    /// Returns the schema name component.
    pub fn schema(&self) -> &Name {
        &self.schema
    }

    /// Returns the unescaped schema name.
    #[must_use]
    pub fn unescaped(&self) -> &str {
        self.schema.unescaped()
    }
}

impl fmt::Display for SchemaName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref db) = self.database {
            write!(f, "{}.{}", db, self.schema)
        } else {
            write!(f, "{}", self.schema)
        }
    }
}

impl TryFrom<&str> for SchemaName {
    type Error = Error;

    fn try_from(s: &str) -> Result<Self> {
        s.parse()
    }
}

impl TryFrom<&String> for SchemaName {
    type Error = Error;

    fn try_from(s: &String) -> Result<Self> {
        s.as_str().parse()
    }
}

impl TryFrom<String> for SchemaName {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Name> for SchemaName {
    fn from(name: Name) -> Self {
        SchemaName {
            database: None,
            schema: name,
        }
    }
}

impl FromStr for SchemaName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let parts = parse_qualified_identifier(s);

        // Parse database.schema format
        match parts.as_slice() {
            [s] => SchemaName::try_new(s),
            [d, s] => SchemaName::try_new(s)?.with_database(d),
            _ => Err(Error::invalid_name(format!("Invalid SQL identifier: {s}"))),
        }
    }
}

/// Represents a fully qualified SQL table name.
///
/// A table name can optionally include database and schema qualifiers.
/// Uses the fluent builder pattern for constructing qualified table names.
///
/// # Example
///
/// ```
/// use hyperdb_api::{TableName, Result};
///
/// # fn main() -> Result<()> {
/// // Simple table name
/// let table = TableName::try_new("users")?;
/// assert_eq!(table.to_string(), "\"users\"");
///
/// // With schema using fluent builder
/// let with_schema = TableName::try_new("users")?.with_schema("public")?;
/// assert_eq!(with_schema.to_string(), "\"public\".\"users\"");
///
/// // Fully qualified using fluent builder
/// let full = TableName::try_new("users")?
///     .with_schema("public")?
///     .with_database("mydb")?;
/// assert_eq!(full.to_string(), "\"mydb\".\"public\".\"users\"");
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[must_use = "TableName represents a validated table identifier that should not be discarded. Use it in your queries or table operations"]
pub struct TableName {
    database: Option<DatabaseName>,
    schema: Option<Name>,
    table: Name,
}

impl TableName {
    /// Creates a new table name without qualifiers (the starting point).
    ///
    /// # Errors
    ///
    /// Returns an error if the table name is empty or exceeds the `PostgreSQL` identifier limit.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableName;
    ///
    /// let table = TableName::try_new("users")?;
    /// assert_eq!(table.to_string(), "\"users\"");
    /// # Ok::<(), hyperdb_api::Error>(())
    /// ```
    pub fn try_new(table: impl Into<String>) -> Result<Self> {
        Ok(TableName {
            database: None,
            schema: None,
            table: Name::try_new(table)?,
        })
    }

    /// Builder method: Sets the schema qualifier.
    ///
    /// This method is part of the fluent builder pattern and can be chained.
    /// Returns `Result<Self>` to allow fallible method chaining.
    ///
    /// # Errors
    ///
    /// Returns an error if the schema name is empty or exceeds the `PostgreSQL` identifier limit.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableName;
    ///
    /// let table = TableName::try_new("users")?.with_schema("public")?;
    /// assert_eq!(table.to_string(), "\"public\".\"users\"");
    /// # Ok::<(), hyperdb_api::Error>(())
    /// ```
    pub fn with_schema(mut self, schema: impl Into<String>) -> Result<Self> {
        self.schema = Some(Name::try_new(schema)?);
        Ok(self)
    }

    /// Builder method: Sets the database qualifier.
    ///
    /// This method is part of the fluent builder pattern and can be chained.
    /// Returns `Result<Self>` to allow fallible method chaining.
    ///
    /// # Errors
    ///
    /// Returns an error if the database name is empty or exceeds the `PostgreSQL` identifier limit.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableName;
    ///
    /// let table = TableName::try_new("users")?
    ///     .with_schema("public")?
    ///     .with_database("mydb")?;
    /// assert_eq!(table.to_string(), "\"mydb\".\"public\".\"users\"");
    /// # Ok::<(), hyperdb_api::Error>(())
    /// ```
    pub fn with_database(mut self, database: impl Into<String>) -> Result<Self> {
        self.database = Some(DatabaseName::try_new(database)?);
        Ok(self)
    }

    /// Returns the database name, if any.
    #[must_use]
    pub fn database(&self) -> Option<&DatabaseName> {
        self.database.as_ref()
    }

    /// Returns the schema name, if any.
    #[must_use]
    pub fn schema(&self) -> Option<&Name> {
        self.schema.as_ref()
    }

    /// Returns the table name component.
    pub fn table(&self) -> &Name {
        &self.table
    }

    /// Returns the unescaped table name.
    #[must_use]
    pub fn unescaped(&self) -> &str {
        self.table.unescaped()
    }
}

impl fmt::Display for TableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref db) = self.database {
            write!(f, "{db}.")?;
        }
        if let Some(ref schema) = self.schema {
            write!(f, "{schema}.")?;
        }
        write!(f, "{}", self.table)
    }
}

impl TryFrom<&str> for TableName {
    type Error = Error;

    fn try_from(s: &str) -> Result<Self> {
        s.parse()
    }
}

impl TryFrom<&String> for TableName {
    type Error = Error;

    fn try_from(s: &String) -> Result<Self> {
        s.as_str().parse()
    }
}

impl TryFrom<String> for TableName {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Name> for TableName {
    fn from(name: Name) -> Self {
        TableName {
            database: None,
            schema: None,
            table: name,
        }
    }
}

impl FromStr for TableName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut chars = s.chars().peekable();

        while let Some(c) = chars.next() {
            match c {
                // Toggle the "in_quotes" state
                '"' => {
                    // Handle escaped quotes (double double-quotes)
                    if in_quotes && chars.peek() == Some(&'"') {
                        current.push('"');
                        chars.next(); // skip the second quote
                    } else {
                        in_quotes = !in_quotes;
                        // Don't add the quote character itself to current
                    }
                }
                // Split on dots, but ONLY if we aren't inside quotes
                '.' if !in_quotes => {
                    if !current.is_empty() {
                        parts.push(current.split_off(0));
                    }
                }
                _ => current.push(c),
            }
        }
        if !current.is_empty() {
            parts.push(current);
        }

        // Now we use the same match logic as before
        match parts.as_slice() {
            [t] => TableName::try_new(t),
            [s, t] => TableName::try_new(t)?.with_schema(s),
            [d, s, t] => TableName::try_new(t)?.with_schema(s)?.with_database(d),
            _ => Err(Error::invalid_name(format!("Invalid SQL identifier: {s}"))),
        }
    }
}

/// Creates a `TableName` with optional database and schema qualifiers.
///
/// This macro provides a convenient way to create table names with different
/// levels of qualification. Returns a `Result` that must be handled with `?` or `.unwrap()`.
///
/// # Examples
///
/// ```
/// use hyperdb_api::table_name;
///
/// // Simple table name
/// let table = table_name!("users")?;
/// assert_eq!(table.to_string(), "\"users\"");
///
/// // With schema
/// let table = table_name!("public", "users")?;
/// assert_eq!(table.to_string(), "\"public\".\"users\"");
///
/// // Fully qualified
/// let table = table_name!("mydb", "public", "users")?;
/// assert_eq!(table.to_string(), "\"mydb\".\"public\".\"users\"");
/// # Ok::<(), hyperdb_api::Error>(())
/// ```
#[macro_export]
macro_rules! table_name {
    // Case: table_name!(db, schema, table)
    ($db:expr, $schema:expr, $table:expr) => {
        $crate::TableName::try_new($table)?
            .with_schema($schema)?
            .with_database($db)
    };

    // Case: table_name!(schema, table)
    ($schema:expr, $table:expr) => {
        $crate::TableName::try_new($table)?.with_schema($schema)
    };

    // Case: table_name!(table)
    ($table:expr) => {
        $crate::TableName::try_new($table)
    };
}

/// Creates a `SchemaName` with optional database qualifier.
///
/// This macro provides a convenient way to create schema names with or without
/// a database qualifier. Returns a `Result` that must be handled with `?` or `.unwrap()`.
///
/// # Examples
///
/// ```
/// use hyperdb_api::schema_name;
///
/// // Simple schema name
/// let schema = schema_name!("public")?;
/// assert_eq!(schema.to_string(), "\"public\"");
///
/// // With database
/// let schema = schema_name!("mydb", "public")?;
/// assert_eq!(schema.to_string(), "\"mydb\".\"public\"");
/// # Ok::<(), hyperdb_api::Error>(())
/// ```
#[macro_export]
macro_rules! schema_name {
    // Case: schema_name!(db, schema)
    ($db:expr, $schema:expr) => {
        $crate::SchemaName::try_new($schema)?.with_database($db)
    };

    // Case: schema_name!(schema)
    ($schema:expr) => {
        $crate::SchemaName::try_new($schema)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_name() {
        assert_eq!(escape_name("table").unwrap(), "\"table\"");
        assert_eq!(escape_name("my_table").unwrap(), "\"my_table\"");
        assert_eq!(escape_name("table\"quote").unwrap(), "\"table\"\"quote\"");
        assert_eq!(escape_name("").unwrap(), "\"\"");
    }

    #[test]
    fn test_escape_name_too_long() {
        // 63 characters should be OK
        let max_name = "a".repeat(PG_IDENTIFIER_LIMIT);
        assert!(escape_name(&max_name).is_ok());

        // 64 characters should fail
        let too_long = "a".repeat(PG_IDENTIFIER_LIMIT + 1);
        let err = escape_name(&too_long).unwrap_err();
        assert!(err.to_string().contains("identifier limit"));
    }

    #[test]
    fn test_escape_sql_path() {
        assert_eq!(escape_sql_path("/tmp/data.hyper"), "\"/tmp/data.hyper\"");
        assert_eq!(
            escape_sql_path("/tmp/my \"db\".hyper"),
            "\"/tmp/my \"\"db\"\".hyper\""
        );
        assert_eq!(escape_sql_path(""), "\"\"");

        // Long paths are allowed (no 63-char limit)
        let long_path = format!("/very/long/path/{}.hyper", "a".repeat(100));
        let escaped = escape_sql_path(&long_path);
        assert!(escaped.starts_with('"'));
        assert!(escaped.ends_with('"'));
    }

    #[test]
    fn test_escape_string_literal() {
        assert_eq!(escape_string_literal("hello"), "'hello'");
        assert_eq!(escape_string_literal("it's"), "'it''s'");
        assert_eq!(escape_string_literal(""), "''");
    }

    #[test]
    fn test_name() {
        let name = Name::try_new("users").unwrap();
        assert_eq!(name.to_string(), "\"users\"");
        assert_eq!(name.unescaped(), "users");
        assert!(!name.unescaped().is_empty());
    }

    #[test]
    fn test_name_with_quotes() {
        let name = Name::try_new("table\"name").unwrap();
        assert_eq!(name.to_string(), "\"table\"\"name\"");
        assert_eq!(name.unescaped(), "table\"name");
    }

    #[test]
    fn test_database_name() {
        let db = DatabaseName::try_new("mydb").unwrap();
        assert_eq!(db.to_string(), "\"mydb\"");
        assert_eq!(db.unescaped(), "mydb");
    }

    #[test]
    fn test_schema_name() {
        let schema = SchemaName::try_new("public").unwrap();
        assert_eq!(schema.to_string(), "\"public\"");

        let qualified = SchemaName::try_new("public")
            .unwrap()
            .with_database("mydb")
            .unwrap();
        assert_eq!(qualified.to_string(), "\"mydb\".\"public\"");
    }

    #[test]
    fn test_table_name() {
        let simple = TableName::try_new("users").unwrap();
        assert_eq!(simple.to_string(), "\"users\"");

        let with_schema = TableName::try_new("users")
            .unwrap()
            .with_schema("public")
            .unwrap();
        assert_eq!(with_schema.to_string(), "\"public\".\"users\"");

        let full = TableName::try_new("users")
            .unwrap()
            .with_schema("public")
            .unwrap()
            .with_database("mydb")
            .unwrap();
        assert_eq!(full.to_string(), "\"mydb\".\"public\".\"users\"");
    }

    #[test]
    fn test_name_equality() {
        let name1 = Name::try_new("test").unwrap();
        let name2 = Name::try_new("test").unwrap();
        let name3 = Name::try_new("other").unwrap();

        assert_eq!(name1, name2);
        assert_ne!(name1, name3);
    }

    #[test]
    fn test_schema_name_from_str() {
        // Simple schema name (using .parse() which uses FromStr)
        let schema: SchemaName = "public".parse().unwrap();
        assert_eq!(schema.to_string(), "\"public\"");
        assert_eq!(schema.unescaped(), "public");

        // Database.schema format
        let qualified: SchemaName = "mydb.public".parse().unwrap();
        assert_eq!(qualified.to_string(), "\"mydb\".\"public\"");
        assert_eq!(qualified.unescaped(), "public");
        assert_eq!(qualified.database().unwrap().unescaped(), "mydb");

        // Quoted identifiers
        let quoted: SchemaName = "\"my db\".\"my schema\"".parse().unwrap();
        assert_eq!(quoted.to_string(), "\"my db\".\"my schema\"");
        assert_eq!(quoted.unescaped(), "my schema");

        // Escaped quotes
        let escaped: SchemaName = "\"schema\"\"name\"".parse().unwrap();
        assert_eq!(escaped.to_string(), "\"schema\"\"name\"");
        assert_eq!(escaped.unescaped(), "schema\"name");

        // Invalid formats (testing FromStr error handling)
        assert!("db.schema.table".parse::<SchemaName>().is_err());
        assert!("".parse::<SchemaName>().is_err());
    }

    #[test]
    fn test_table_name_from_str() {
        // Simple table name (using .parse() which uses FromStr)
        let table: TableName = "users".parse().unwrap();
        assert_eq!(table.to_string(), "\"users\"");
        assert_eq!(table.unescaped(), "users");

        // Schema.table format
        let with_schema: TableName = "public.users".parse().unwrap();
        assert_eq!(with_schema.to_string(), "\"public\".\"users\"");
        assert_eq!(with_schema.unescaped(), "users");
        assert_eq!(with_schema.schema().unwrap().unescaped(), "public");

        // Database.schema.table format
        let full: TableName = "mydb.public.users".parse().unwrap();
        assert_eq!(full.to_string(), "\"mydb\".\"public\".\"users\"");
        assert_eq!(full.unescaped(), "users");
        assert_eq!(full.schema().unwrap().unescaped(), "public");
        assert_eq!(full.database().unwrap().unescaped(), "mydb");

        // Quoted identifiers
        let quoted: TableName = "\"my db\".\"my schema\".\"my table\"".parse().unwrap();
        assert_eq!(quoted.to_string(), "\"my db\".\"my schema\".\"my table\"");
        assert_eq!(quoted.unescaped(), "my table");

        // Escaped quotes
        let escaped: TableName = "\"table\"\"name\"".parse().unwrap();
        assert_eq!(escaped.to_string(), "\"table\"\"name\"");
        assert_eq!(escaped.unescaped(), "table\"name");

        // Dots inside quoted identifiers should not split
        let with_dots: TableName = "\"schema.name\".\"table.name\"".parse().unwrap();
        assert_eq!(with_dots.to_string(), "\"schema.name\".\"table.name\"");
        assert_eq!(with_dots.schema().unwrap().unescaped(), "schema.name");
        assert_eq!(with_dots.unescaped(), "table.name");

        // Invalid formats (testing FromStr error handling)
        assert!("db.schema.table.extra".parse::<TableName>().is_err());
        assert!("".parse::<TableName>().is_err());
    }

    #[test]
    fn test_schema_name_macro() -> Result<()> {
        // Simple schema name
        let schema = schema_name!("public")?;
        assert_eq!(schema.to_string(), "\"public\"");
        assert_eq!(schema.unescaped(), "public");

        // With database
        let qualified = schema_name!("mydb", "public")?;
        assert_eq!(qualified.to_string(), "\"mydb\".\"public\"");
        assert_eq!(qualified.unescaped(), "public");
        assert_eq!(qualified.database().unwrap().unescaped(), "mydb");
        Ok(())
    }

    #[test]
    fn test_table_name_macro() -> Result<()> {
        // Simple table name
        let table = table_name!("users")?;
        assert_eq!(table.to_string(), "\"users\"");
        assert_eq!(table.unescaped(), "users");

        // With schema
        let with_schema = table_name!("public", "users")?;
        assert_eq!(with_schema.to_string(), "\"public\".\"users\"");
        assert_eq!(with_schema.unescaped(), "users");
        assert_eq!(with_schema.schema().unwrap().unescaped(), "public");

        // Fully qualified
        let full = table_name!("mydb", "public", "users")?;
        assert_eq!(full.to_string(), "\"mydb\".\"public\".\"users\"");
        assert_eq!(full.unescaped(), "users");
        assert_eq!(full.schema().unwrap().unescaped(), "public");
        assert_eq!(full.database().unwrap().unescaped(), "mydb");
        Ok(())
    }

    #[test]
    fn test_schema_name_try_from() {
        // Simple schema name using TryFrom
        let schema: SchemaName = "public".try_into().unwrap();
        assert_eq!(schema.to_string(), "\"public\"");
        assert_eq!(schema.unescaped(), "public");

        // Qualified schema using TryFrom (parses dot-separated format)
        let qualified: SchemaName = "mydb.public".try_into().unwrap();
        assert_eq!(qualified.to_string(), "\"mydb\".\"public\"");
        assert_eq!(qualified.unescaped(), "public");
        assert_eq!(qualified.database().unwrap().unescaped(), "mydb");

        // From String
        let schema_string: SchemaName = String::from("public").try_into().unwrap();
        assert_eq!(schema_string.to_string(), "\"public\"");
    }

    #[test]
    fn test_table_name_try_from() {
        // Simple table name using TryFrom
        let table: TableName = "users".try_into().unwrap();
        assert_eq!(table.to_string(), "\"users\"");
        assert_eq!(table.unescaped(), "users");

        // With schema using TryFrom (parses dot-separated format)
        let with_schema: TableName = "public.users".try_into().unwrap();
        assert_eq!(with_schema.to_string(), "\"public\".\"users\"");
        assert_eq!(with_schema.unescaped(), "users");
        assert_eq!(with_schema.schema().unwrap().unescaped(), "public");

        // Fully qualified using TryFrom (parses dot-separated format)
        let full: TableName = "mydb.public.users".try_into().unwrap();
        assert_eq!(full.to_string(), "\"mydb\".\"public\".\"users\"");
        assert_eq!(full.unescaped(), "users");
        assert_eq!(full.schema().unwrap().unescaped(), "public");
        assert_eq!(full.database().unwrap().unescaped(), "mydb");

        // From String
        let table_string: TableName = String::from("users").try_into().unwrap();
        assert_eq!(table_string.to_string(), "\"users\"");

        // Invalid format returns error
        let invalid: std::result::Result<TableName, _> = "db.schema.table.extra".try_into();
        assert!(invalid.is_err());
    }
}
