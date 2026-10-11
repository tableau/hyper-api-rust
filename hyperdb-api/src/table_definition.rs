// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Table definition types.

use std::borrow::Cow;

use crate::error::{Error, Result};
use hyperdb_api_core::types::{Nullability, SqlType};

/// Possible persistence levels for database objects.
///
/// This enum controls whether a table is permanent (persisted to disk) or
/// temporary (only available in the current session).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Persistence {
    /// Permanent: The table is persisted to disk and survives session restarts.
    #[default]
    Permanent,
    /// Temporary: The table only exists for the current session and is not persisted.
    Temporary,
}

impl std::fmt::Display for Persistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Persistence::Permanent => write!(f, "Permanent"),
            Persistence::Temporary => write!(f, "Temporary"),
        }
    }
}

/// Internal representation of a column's SQL type.
///
/// This enum ensures a single source of truth for the type - either a structured
/// `SqlType` or a raw string type name.
#[derive(Debug, Clone)]
enum SqlTypeOrName {
    /// Structured SQL type with full type information.
    SqlType(SqlType),
    /// Raw string type name (used when `SqlType` is unavailable, e.g., from catalog queries).
    TypeName(String),
}

impl SqlTypeOrName {
    /// Returns the type name as a string.
    ///
    /// Returns a borrowed reference for `TypeName` variant to avoid allocation,
    /// and an owned string for `SqlType` variant (requires formatting).
    fn type_name(&self) -> Cow<'_, str> {
        match self {
            SqlTypeOrName::SqlType(t) => Cow::Owned(t.to_string()),
            SqlTypeOrName::TypeName(s) => Cow::Borrowed(s),
        }
    }

    /// Returns the SQL type if this is a structured type.
    fn sql_type(&self) -> Option<SqlType> {
        match self {
            SqlTypeOrName::SqlType(t) => Some(*t),
            SqlTypeOrName::TypeName(_) => None,
        }
    }
}

/// A table-level constraint.
///
/// Hyper accepts only the *assumed* constraint forms on `CREATE TABLE`. Real
/// `PRIMARY KEY`, `UNIQUE`, and `FOREIGN KEY` clauses are rejected with
/// `Index support is disabled`, and `CHECK` with `check constraints not
/// implemented yet`, so they can never appear on a Hyper table in the first
/// place. Assumed constraints are declarations the optimizer trusts but the
/// engine does **not** enforce — inserting a duplicate key succeeds.
///
/// Hyper also rejects `CONSTRAINT <name> …` (`named constraints not
/// implemented yet`), so constraints carry no user-chosen name; the engine
/// derives one from the table and column names.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableConstraint {
    /// `ASSUMED PRIMARY KEY (cols…)`. Every listed column must be `NOT NULL`.
    AssumedPrimaryKey {
        /// Key columns, in declaration order.
        columns: Vec<String>,
    },
    /// `ASSUMED UNIQUE (cols…)`.
    AssumedUnique {
        /// Key columns, in declaration order.
        columns: Vec<String>,
    },
}

impl TableConstraint {
    /// Returns the columns the constraint covers, in declaration order.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        match self {
            Self::AssumedPrimaryKey { columns } | Self::AssumedUnique { columns } => columns,
        }
    }

    /// Renders the constraint as the SQL clause used inside `CREATE TABLE`.
    fn to_sql(&self) -> String {
        let keyword = match self {
            Self::AssumedPrimaryKey { .. } => "ASSUMED PRIMARY KEY",
            Self::AssumedUnique { .. } => "ASSUMED UNIQUE",
        };
        // Quoted unconditionally, for the same reason as the column list in
        // `to_create_sql`: a constrained column may be a reserved word.
        let cols = self
            .columns()
            .iter()
            .map(|c| QuotedIdentifier(c).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{keyword} ({cols})")
    }
}

/// A column definition.
///
/// This struct supports both string-based type names (for simplicity) and
/// SqlType-based definitions (for type safety). Internally, it uses a single source
/// of truth to avoid synchronization issues.
#[derive(Debug, Clone)]
pub struct ColumnDefinition {
    /// Column name.
    pub name: String,
    /// SQL type representation (either structured `SqlType` or raw string).
    sql_type_or_name: SqlTypeOrName,
    /// Whether the column is nullable.
    pub nullable: bool,
    /// The collation for text columns (e.g., "`en_US`", "binary").
    collation: Option<String>,
    /// The `DEFAULT` expression, as SQL source text.
    default_expr: Option<String>,
}

impl ColumnDefinition {
    /// Creates a new column definition using a type name string.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::ColumnDefinition;
    ///
    /// let col = ColumnDefinition::new("id", "INT", false);
    /// assert_eq!(col.name, "id");
    /// assert_eq!(col.type_name(), "INT");
    /// ```
    pub fn new(name: impl Into<String>, type_name: impl Into<String>, nullable: bool) -> Self {
        ColumnDefinition {
            name: name.into(),
            sql_type_or_name: SqlTypeOrName::TypeName(type_name.into()),
            nullable,
            collation: None,
            default_expr: None,
        }
    }

    /// Creates a column definition using `SqlType`.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::ColumnDefinition;
    /// use hyperdb_api_core::types::{SqlType, Nullability};
    ///
    /// let col = ColumnDefinition::with_sql_type("id", SqlType::int(), Nullability::NotNullable);
    /// assert_eq!(col.name, "id");
    /// assert!(!col.nullable);
    /// ```
    pub fn with_sql_type(
        name: impl Into<String>,
        sql_type: SqlType,
        nullability: Nullability,
    ) -> Self {
        ColumnDefinition {
            name: name.into(),
            sql_type_or_name: SqlTypeOrName::SqlType(sql_type),
            nullable: nullability.is_nullable(),
            collation: None,
            default_expr: None,
        }
    }

    /// Creates a column definition with a collation.
    ///
    /// The collation specifies the sorting and comparison behavior for text columns.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::ColumnDefinition;
    /// use hyperdb_api_core::types::{SqlType, Nullability};
    ///
    /// let col = ColumnDefinition::with_collation("name", SqlType::text(), "en_US", Nullability::Nullable);
    /// assert_eq!(col.collation(), Some("en_US"));
    /// ```
    pub fn with_collation(
        name: impl Into<String>,
        sql_type: SqlType,
        collation: impl Into<String>,
        nullability: Nullability,
    ) -> Self {
        ColumnDefinition {
            name: name.into(),
            sql_type_or_name: SqlTypeOrName::SqlType(sql_type),
            nullable: nullability.is_nullable(),
            collation: Some(collation.into()),
            default_expr: None,
        }
    }

    /// Creates a nullable column definition using `SqlType`.
    pub fn nullable(name: impl Into<String>, sql_type: SqlType) -> Self {
        Self::with_sql_type(name, sql_type, Nullability::Nullable)
    }

    /// Creates a non-nullable column definition using `SqlType`.
    pub fn not_null(name: impl Into<String>, sql_type: SqlType) -> Self {
        Self::with_sql_type(name, sql_type, Nullability::NotNullable)
    }

    /// Returns the nullability as a Nullability enum.
    #[must_use]
    pub fn nullability(&self) -> Nullability {
        if self.nullable {
            Nullability::Nullable
        } else {
            Nullability::NotNullable
        }
    }

    /// Returns the SQL type if this column was created with a structured type.
    #[must_use]
    pub fn sql_type(&self) -> Option<SqlType> {
        self.sql_type_or_name.sql_type()
    }

    /// Returns the type name string representation.
    ///
    /// When created with `SqlType`, this is derived from it. Otherwise, it's the
    /// string provided during construction.
    ///
    /// Returns `Cow<str>` to avoid allocation when the type name is already stored
    /// as a string internally.
    #[must_use]
    pub fn type_name(&self) -> Cow<'_, str> {
        self.sql_type_or_name.type_name()
    }

    /// Returns the collation if set.
    #[must_use]
    pub fn collation(&self) -> Option<&str> {
        self.collation.as_deref()
    }

    /// Returns the `DEFAULT` expression, as SQL source text, if the column has one.
    ///
    /// The text is a SQL expression (`42`, `'n/a'`, `NOW()`), not a value — it
    /// is rendered verbatim into `CREATE TABLE`.
    #[must_use]
    pub fn default_expr(&self) -> Option<&str> {
        self.default_expr.as_deref()
    }

    /// Sets the `DEFAULT` expression from SQL source text.
    ///
    /// The expression is emitted verbatim, so it must be valid SQL in the
    /// database the table is created in. See [`TableDefinition::to_create_sql`].
    pub fn set_default_expr(&mut self, expr: impl Into<String>) {
        self.default_expr = Some(expr.into());
    }

    /// Removes the `DEFAULT` expression.
    pub fn clear_default_expr(&mut self) {
        self.default_expr = None;
    }

    /// Sets the collation for this column.
    pub fn set_collation(&mut self, collation: impl Into<String>) {
        self.collation = Some(collation.into());
    }

    /// Sets the SQL type, replacing any previous type information.
    ///
    /// This replaces the internal type representation with the provided `SqlType`.
    pub fn set_sql_type(&mut self, sql_type: SqlType) {
        self.sql_type_or_name = SqlTypeOrName::SqlType(sql_type);
    }
}

/// A table definition.
///
/// This struct defines the schema of a table including its name, optional schema
/// and database names, and column definitions.
///
/// # Example
///
/// Using the fluent builder pattern:
///
/// ```
/// use hyperdb_api::{TableDefinition, Result};
/// use hyperdb_api_core::types::{SqlType, Nullability};
///
/// # fn main() -> Result<()> {
/// let table = TableDefinition::new("users")
///     .add_required_column("id", SqlType::int())
///     .add_nullable_column("name", SqlType::text());
///
/// let sql = table.to_create_sql(true)?;
/// assert!(sql.contains("CREATE TABLE"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
#[must_use = "TableDefinition uses a consuming builder pattern - each method takes ownership and returns a new instance. You must use the returned value or your table definition changes will be lost"]
pub struct TableDefinition {
    /// Table name.
    pub name: String,
    /// Schema name.
    pub schema: Option<String>,
    /// Database name.
    pub database: Option<String>,
    /// Column definitions.
    pub columns: Vec<ColumnDefinition>,
    /// Table persistence (permanent or temporary).
    persistence: Persistence,
    /// Table-level constraints.
    constraints: Vec<TableConstraint>,
}

impl Default for TableDefinition {
    fn default() -> Self {
        Self::new("unnamed_table")
    }
}

impl From<&str> for TableDefinition {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl From<String> for TableDefinition {
    fn from(name: String) -> Self {
        Self::new(name)
    }
}

impl TableDefinition {
    /// Creates a new table definition.
    pub fn new(name: impl Into<String>) -> Self {
        TableDefinition {
            name: name.into(),
            schema: None,
            database: None,
            columns: Vec::new(),
            persistence: Persistence::Permanent,
            constraints: Vec::new(),
        }
    }

    /// Creates a table definition from a validated `TableName`.
    ///
    /// This constructor uses a pre-validated `TableName`, ensuring all name components
    /// have already passed validation (non-empty, within length limits).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::{TableDefinition, TableName};
    /// use hyperdb_api_core::types::{SqlType, Nullability};
    ///
    /// // First create a validated TableName
    /// let table_name = TableName::try_new("users")?
    ///     .with_schema("public")?
    ///     .with_database("mydb")?;
    ///
    /// // Then create TableDefinition from it
    /// let table = TableDefinition::from_table_name(table_name)?
    ///     .add_required_column("id", SqlType::int());
    ///
    /// assert_eq!(table.name, "users");
    /// assert_eq!(table.schema, Some("public".to_string()));
    /// assert_eq!(table.database, Some("mydb".to_string()));
    ///
    /// // Direct conversion from string also works
    /// let table2 = TableDefinition::from_table_name("public.users")?;
    /// assert_eq!(table2.schema, Some("public".to_string()));
    /// # Ok::<(), hyperdb_api::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the conversion error (typically [`Error::InvalidName`]) if
    /// `table_name` cannot be parsed into a
    /// [`TableName`](crate::TableName).
    pub fn from_table_name<T>(table_name: T) -> Result<Self>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let table_name = table_name.try_into()?;
        Ok(TableDefinition {
            name: table_name.table().unescaped().to_string(),
            schema: table_name.schema().map(|s| s.unescaped().to_string()),
            database: table_name.database().map(|d| d.unescaped().to_string()),
            columns: Vec::new(),
            persistence: Persistence::Permanent,
            constraints: Vec::new(),
        })
    }

    /// Sets the schema name (fluent builder pattern).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableDefinition;
    /// use hyperdb_api_core::types::{SqlType, Nullability};
    ///
    /// let table = TableDefinition::new("Extract")
    ///     .with_schema("Extract")
    ///     .add_required_column("id", SqlType::int());
    /// ```
    pub fn with_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    /// Sets the database name (fluent builder pattern).
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = Some(database.into());
        self
    }

    /// Sets the persistence (fluent builder pattern).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::{TableDefinition, Persistence};
    /// use hyperdb_api_core::types::SqlType;
    ///
    /// let temp_table = TableDefinition::new("temp_data")
    ///     .with_persistence(Persistence::Temporary)
    ///     .add_required_column("id", SqlType::int());
    /// assert_eq!(temp_table.get_persistence(), Persistence::Temporary);
    /// ```
    pub fn with_persistence(mut self, persistence: Persistence) -> Self {
        self.persistence = persistence;
        self
    }

    /// Returns the persistence setting.
    #[must_use]
    pub fn get_persistence(&self) -> Persistence {
        self.persistence
    }

    /// Sets the persistence.
    pub fn set_persistence(&mut self, persistence: Persistence) {
        self.persistence = persistence;
    }

    /// Adds a column to the table definition (fluent builder pattern).
    ///
    /// This is an internal method. Use `add_nullable_column()` or `add_required_column()` instead.
    #[expect(
        dead_code,
        reason = "called from the `table!` declarative macro; not invoked by the crate itself"
    )]
    pub(crate) fn add_column(
        mut self,
        name: impl Into<String>,
        sql_type: SqlType,
        nullability: Nullability,
    ) -> Self {
        self.columns
            .push(ColumnDefinition::with_sql_type(name, sql_type, nullability));
        self
    }

    /// Adds a nullable column using `SqlType` (fluent builder pattern).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableDefinition;
    /// use hyperdb_api_core::types::SqlType;
    ///
    /// let table = TableDefinition::new("products")
    ///     .add_nullable_column("name", SqlType::text())
    ///     .add_nullable_column("price", SqlType::numeric(18, 2));
    /// ```
    pub fn add_nullable_column(mut self, name: impl Into<String>, sql_type: SqlType) -> Self {
        self.columns.push(ColumnDefinition::with_sql_type(
            name,
            sql_type,
            Nullability::Nullable,
        ));
        self
    }

    /// Adds a required (non-nullable) column using `SqlType` (fluent builder pattern).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableDefinition;
    /// use hyperdb_api_core::types::SqlType;
    ///
    /// let table = TableDefinition::new("products")
    ///     .add_required_column("id", SqlType::int())
    ///     .add_required_column("name", SqlType::text());
    /// ```
    pub fn add_required_column(mut self, name: impl Into<String>, sql_type: SqlType) -> Self {
        self.columns.push(ColumnDefinition::with_sql_type(
            name,
            sql_type,
            Nullability::NotNullable,
        ));
        self
    }

    /// Adds a column with a collation (fluent builder pattern).
    ///
    /// This is an internal method. Use `add_nullable_column_with_collation()` or `add_required_column_with_collation()` instead.
    #[expect(
        dead_code,
        reason = "called from the `table!` declarative macro; not invoked by the crate itself"
    )]
    pub(crate) fn add_column_with_collation(
        mut self,
        name: impl Into<String>,
        sql_type: SqlType,
        collation: impl Into<String>,
        nullability: Nullability,
    ) -> Self {
        self.columns.push(ColumnDefinition::with_collation(
            name,
            sql_type,
            collation,
            nullability,
        ));
        self
    }

    /// Adds a nullable column with a collation (fluent builder pattern).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableDefinition;
    /// use hyperdb_api_core::types::SqlType;
    ///
    /// let table = TableDefinition::new("products")
    ///     .add_nullable_column_with_collation("name", SqlType::text(), "en_US");
    /// ```
    pub fn add_nullable_column_with_collation(
        mut self,
        name: impl Into<String>,
        sql_type: SqlType,
        collation: impl Into<String>,
    ) -> Self {
        self.columns.push(ColumnDefinition::with_collation(
            name,
            sql_type,
            collation,
            Nullability::Nullable,
        ));
        self
    }

    /// Adds a required (non-nullable) column with a collation (fluent builder pattern).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableDefinition;
    /// use hyperdb_api_core::types::SqlType;
    ///
    /// let table = TableDefinition::new("products")
    ///     .add_required_column_with_collation("name", SqlType::text(), "en_US");
    /// ```
    pub fn add_required_column_with_collation(
        mut self,
        name: impl Into<String>,
        sql_type: SqlType,
        collation: impl Into<String>,
    ) -> Self {
        self.columns.push(ColumnDefinition::with_collation(
            name,
            sql_type,
            collation,
            Nullability::NotNullable,
        ));
        self
    }

    /// Adds a `ColumnDefinition` directly (fluent builder pattern).
    pub fn add_column_def(mut self, column: ColumnDefinition) -> Self {
        self.columns.push(column);
        self
    }

    /// Adds a column with raw type string (internal use).
    #[expect(
        dead_code,
        reason = "retained for catalog reflection paths that pass string type names"
    )]
    pub(crate) fn add_column_raw(&mut self, name: &str, type_name: &str, nullable: bool) {
        // Map the Hyper type name to SqlType if possible
        let sql_type_or_name = Self::type_name_to_sql_type(type_name).map_or_else(
            || SqlTypeOrName::TypeName(type_name.to_string()),
            SqlTypeOrName::SqlType,
        );

        self.columns.push(ColumnDefinition {
            name: name.to_string(),
            sql_type_or_name,
            nullable,
            collation: None,
            default_expr: None,
        });
    }

    /// Adds a column with a pre-constructed `SqlType` (internal use).
    ///
    /// This is used by the catalog when it has OID and type modifier information
    /// to construct the proper `SqlType` with precision/scale.
    pub(crate) fn add_column_with_sql_type(
        &mut self,
        name: &str,
        sql_type: SqlType,
        nullable: bool,
    ) {
        self.columns.push(ColumnDefinition {
            name: name.to_string(),
            sql_type_or_name: SqlTypeOrName::SqlType(sql_type),
            nullable,
            collation: None,
            default_expr: None,
        });
    }

    /// Maps a Hyper type name from `pg_type` to `SqlType`.
    #[allow(
        dead_code,
        reason = "helper used only by `add_column_raw`, which is itself gated on macro use"
    )]
    fn type_name_to_sql_type(type_name: &str) -> Option<SqlType> {
        // Hyper uses PostgreSQL-style type names
        match type_name.to_lowercase().as_str() {
            "integer" | "int4" | "int" => Some(SqlType::int()),
            "smallint" | "int2" => Some(SqlType::small_int()),
            "bigint" | "int8" => Some(SqlType::big_int()),
            "double precision" | "float8" => Some(SqlType::double()),
            "real" | "float4" => Some(SqlType::float()),
            "text" => Some(SqlType::text()),
            "boolean" | "bool" => Some(SqlType::bool()),
            "date" => Some(SqlType::date()),
            "time" | "time without time zone" => Some(SqlType::time()),
            "timestamp" | "timestamp without time zone" => Some(SqlType::timestamp()),
            "timestamptz" | "timestamp with time zone" => Some(SqlType::timestamp_tz()),
            "bytea" => Some(SqlType::bytes()),
            "numeric" => Some(SqlType::numeric(38, 0)), // Default precision/scale
            "json" => Some(SqlType::json()),
            "geography" => Some(SqlType::tabgeography()),
            s if s.starts_with("varchar") || s.starts_with("character varying") => {
                Some(SqlType::varchar(Some(1000))) // Default max length
            }
            s if s.starts_with("char") || s.starts_with("character") => {
                Some(SqlType::char(1)) // Default length
            }
            _ => None,
        }
    }

    /// Returns the table-level constraints.
    #[must_use]
    pub fn constraints(&self) -> &[TableConstraint] {
        &self.constraints
    }

    /// Adds a table-level constraint in place.
    pub fn push_constraint(&mut self, constraint: TableConstraint) {
        self.constraints.push(constraint);
    }

    /// Replaces the table-level constraints.
    pub fn set_constraints(&mut self, constraints: Vec<TableConstraint>) {
        self.constraints = constraints;
    }

    /// Returns the number of columns.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Returns the column definitions.
    #[must_use]
    pub fn columns(&self) -> &[ColumnDefinition] {
        &self.columns
    }

    /// Returns the column at the given position.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds.
    #[must_use]
    pub fn column(&self, index: usize) -> &ColumnDefinition {
        &self.columns[index]
    }

    /// Returns the column with the given name, if it exists.
    #[must_use]
    pub fn column_by_name(&self, name: &str) -> Option<&ColumnDefinition> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Returns the position of the column with the given name.
    #[must_use]
    pub fn column_position_by_name(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Returns the table name (unqualified, escaped).
    ///
    /// This returns just the table name portion, properly escaped for use in SQL.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableDefinition;
    ///
    /// let table = TableDefinition::new("Extract").with_schema("Extract");
    /// // Names are always quoted, preserving case and allowing reserved words
    /// assert_eq!(table.table_name(), "\"Extract\"");
    /// ```
    #[must_use]
    pub fn table_name(&self) -> String {
        format!("{}", SqlIdentifier(&self.name))
    }

    /// Returns the schema name (escaped), if set.
    #[must_use]
    pub fn schema_name(&self) -> Option<String> {
        self.schema
            .as_ref()
            .map(|s| format!("{}", SqlIdentifier(s)))
    }

    /// Returns the database name (escaped), if set.
    #[must_use]
    pub fn database_name(&self) -> Option<String> {
        self.database
            .as_ref()
            .map(|s| format!("{}", SqlIdentifier(s)))
    }

    /// Returns the qualified table name (escaped).
    ///
    /// Format: `database.schema.table`, with every part double-quoted so that
    /// reserved words such as `order` and mixed-case names are valid SQL.
    #[must_use]
    pub fn qualified_name(&self) -> String {
        match (&self.database, &self.schema) {
            (Some(db), Some(schema)) => format!(
                "{}.{}.{}",
                SqlIdentifier(db),
                SqlIdentifier(schema),
                SqlIdentifier(&self.name)
            ),
            (None, Some(schema)) => {
                format!("{}.{}", SqlIdentifier(schema), SqlIdentifier(&self.name))
            }
            (Some(db), None) => format!("{}.{}", SqlIdentifier(db), SqlIdentifier(&self.name)),
            (None, None) => format!("{}", SqlIdentifier(&self.name)),
        }
    }

    /// Sets the table name.
    pub fn set_table_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// Converts this `TableDefinition` to a validated `TableName`.
    ///
    /// This method validates all name components (table, schema, database) and returns
    /// a type-safe `TableName`. Use this when you need to ensure the names are valid.
    ///
    /// # Errors
    ///
    /// Returns an error if any name component is empty or exceeds the `PostgreSQL` identifier limit.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::TableDefinition;
    ///
    /// let table = TableDefinition::new("users")
    ///     .with_schema("public")
    ///     .with_database("mydb");
    ///
    /// // Validate all names
    /// let table_name = table.to_table_name()?;
    /// assert_eq!(table_name.to_string(), "\"mydb\".\"public\".\"users\"");
    /// # Ok::<(), hyperdb_api::Error>(())
    /// ```
    pub fn to_table_name(&self) -> Result<crate::TableName> {
        let mut table = crate::TableName::try_new(&self.name)?;
        if let Some(ref schema) = self.schema {
            table = table.with_schema(schema)?;
        }
        if let Some(ref database) = self.database {
            table = table.with_database(database)?;
        }
        Ok(table)
    }

    /// Generates CREATE TABLE SQL.
    ///
    /// # Arguments
    ///
    /// * `fail_if_exists` - If true, the statement will fail if the table exists.
    ///   If false, uses CREATE TABLE IF NOT EXISTS.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api::{TableDefinition, Result};
    /// use hyperdb_api_core::types::{SqlType, Nullability};
    ///
    /// # fn main() -> Result<()> {
    /// let table = TableDefinition::new("users")
    ///     .add_required_column("id", SqlType::int());
    ///
    /// // Identifiers are quoted unconditionally, so that a name which happens
    /// // to be a SQL reserved word is still emitted correctly.
    /// let sql = table.to_create_sql(true)?;
    /// assert_eq!(sql, r#"CREATE TABLE "users" ("id" INTEGER NOT NULL)"#);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTableDefinition`] with message
    /// `"Table must have at least one column"` if this definition has no
    /// columns.
    pub fn to_create_sql(&self, fail_if_exists: bool) -> Result<String> {
        if self.columns.is_empty() {
            return Err(Error::invalid_table_definition(
                "Table must have at least one column",
            ));
        }

        let mut sql = String::new();

        // Handle temporary tables
        let create_keyword = match self.persistence {
            Persistence::Permanent => {
                if fail_if_exists {
                    "CREATE TABLE "
                } else {
                    "CREATE TABLE IF NOT EXISTS "
                }
            }
            Persistence::Temporary => {
                if fail_if_exists {
                    "CREATE TEMPORARY TABLE "
                } else {
                    "CREATE TEMPORARY TABLE IF NOT EXISTS "
                }
            }
        };

        sql.push_str(create_keyword);
        sql.push_str(&self.qualified_name());
        sql.push_str(" (");

        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }

            // Quote column names unconditionally. This preserves case
            // (Hyper case-folds unquoted identifiers to lowercase) and, just
            // as importantly, survives names that are reserved words:
            // `is_valid_unquoted_identifier` does not know the keyword list,
            // so `SqlIdentifier` would emit a lowercase `select` bare and the
            // engine would reject the statement.
            // Note: write! to String is infallible, so we can ignore the Result
            let _ = write!(sql, "{} {}", QuotedIdentifier(&col.name), col.type_name());

            // Add collation if specified. Also always quoted — collation names
            // are case-sensitive (`en_US`) and the engine accepts the quoted
            // form for every name in pg_collation.
            if let Some(collation) = &col.collation {
                let _ = write!(sql, " COLLATE {}", QuotedIdentifier(collation));
            }

            if !col.nullable {
                sql.push_str(" NOT NULL");
            }

            if let Some(default_expr) = &col.default_expr {
                let _ = write!(sql, " DEFAULT {default_expr}");
            }
        }

        for constraint in &self.constraints {
            let _ = write!(sql, ", {}", constraint.to_sql());
        }

        sql.push(')');

        Ok(sql)
    }

    /// Generates DROP TABLE SQL.
    ///
    /// # Arguments
    ///
    /// * `fail_if_not_exists` - If true, the statement will fail if the table doesn't exist.
    ///   If false, uses DROP TABLE IF EXISTS.
    #[must_use]
    pub fn to_drop_sql(&self, fail_if_not_exists: bool) -> String {
        let mut sql = String::new();

        if fail_if_not_exists {
            sql.push_str("DROP TABLE ");
        } else {
            sql.push_str("DROP TABLE IF EXISTS ");
        }

        sql.push_str(&self.qualified_name());
        sql
    }
}

use hyperdb_api_core::protocol::escape::{QuotedIdentifier, SqlIdentifier};
use std::fmt::Write;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_sql() {
        let table = TableDefinition::new("users")
            .add_required_column("id", SqlType::int())
            .add_nullable_column("name", SqlType::text());

        // Identifiers are quoted unconditionally: the bare-identifier check
        // does not know the reserved word list, so a name like `order` would
        // otherwise be emitted bare and rejected. Quoting a name that did not
        // need it means the same thing.
        let sql = table.to_create_sql(true).unwrap();
        assert_eq!(
            sql,
            r#"CREATE TABLE "users" ("id" INTEGER NOT NULL, "name" TEXT)"#
        );

        // Verify type_name accessor works
        assert_eq!(table.columns[0].type_name(), "INTEGER");
        assert_eq!(table.columns[1].type_name(), "TEXT");
    }

    #[test]
    fn create_sql_quotes_reserved_words() {
        // `is_valid_unquoted_identifier` has no reserved word list, so these
        // names look like legal bare identifiers. Emitting them bare produces
        // `syntax error: got SELECT`, which is how a whole-database copy used
        // to die on a table it had faithfully reflected.
        let table = TableDefinition::new("order")
            .add_required_column("select", SqlType::int())
            .add_nullable_column("from", SqlType::text());

        assert_eq!(
            table.to_create_sql(true).unwrap(),
            r#"CREATE TABLE "order" ("select" INTEGER NOT NULL, "from" TEXT)"#
        );
    }

    #[test]
    fn create_sql_quotes_constraint_columns() {
        let mut table = TableDefinition::new("t").add_required_column("select", SqlType::int());
        table.push_constraint(TableConstraint::AssumedPrimaryKey {
            columns: vec!["select".to_string()],
        });

        assert!(
            table
                .to_create_sql(true)
                .unwrap()
                .contains(r#"ASSUMED PRIMARY KEY ("select")"#)
        );
    }

    #[test]
    fn test_create_sql_with_numeric() {
        let table = TableDefinition::new("products")
            .add_required_column("id", SqlType::int())
            .add_nullable_column("name", SqlType::text())
            .add_nullable_column("price", SqlType::numeric(18, 2));

        let sql = table.to_create_sql(true).unwrap();
        assert_eq!(
            sql,
            r#"CREATE TABLE "products" ("id" INTEGER NOT NULL, "name" TEXT, "price" NUMERIC(18, 2))"#
        );
    }

    #[test]
    fn test_qualified_name() {
        let table = TableDefinition::new("users")
            .with_schema("public")
            .with_database("mydb");
        assert_eq!(table.qualified_name(), r#""mydb"."public"."users""#);
    }

    #[test]
    fn test_table_name() {
        let table = TableDefinition::new("Extract").with_schema("Extract");
        // Names are always quoted, preserving case and allowing reserved words
        assert_eq!(table.table_name(), r#""Extract""#);
    }

    #[test]
    fn test_drop_sql() {
        let table = TableDefinition::new("users");
        assert_eq!(table.to_drop_sql(true), r#"DROP TABLE "users""#);
        assert_eq!(table.to_drop_sql(false), r#"DROP TABLE IF EXISTS "users""#);
    }

    #[test]
    fn test_column_definition_helpers() {
        let not_null = ColumnDefinition::not_null("id", SqlType::int());
        assert!(!not_null.nullable);

        let nullable = ColumnDefinition::nullable("name", SqlType::text());
        assert!(nullable.nullable);
    }

    #[test]
    fn test_persistence() {
        let perm = TableDefinition::new("data");
        assert_eq!(perm.get_persistence(), Persistence::Permanent);

        let temp = TableDefinition::new("temp_data").with_persistence(Persistence::Temporary);
        assert_eq!(temp.get_persistence(), Persistence::Temporary);
    }

    #[test]
    fn test_temporary_table_sql() {
        let table = TableDefinition::new("temp_data")
            .with_persistence(Persistence::Temporary)
            .add_required_column("id", SqlType::int());

        let sql = table.to_create_sql(true).unwrap();
        assert_eq!(
            sql,
            r#"CREATE TEMPORARY TABLE "temp_data" ("id" INTEGER NOT NULL)"#
        );
    }

    #[test]
    fn test_collation() {
        let col = ColumnDefinition::with_collation(
            "name",
            SqlType::text(),
            "en_US",
            Nullability::Nullable,
        );
        assert_eq!(col.collation(), Some("en_US"));
    }

    #[test]
    fn test_column_with_collation_sql() {
        let table = TableDefinition::new("users").add_nullable_column_with_collation(
            "name",
            SqlType::text(),
            "en_US",
        );

        let sql = table.to_create_sql(true).unwrap();
        // "en_US" is quoted because it contains uppercase letters (to preserve case)
        assert!(sql.contains(r#"COLLATE "en_US""#));
    }

    #[test]
    fn test_column_lookup() {
        let table = TableDefinition::new("users")
            .add_required_column("id", SqlType::int())
            .add_nullable_column("name", SqlType::text());

        assert!(table.column_by_name("id").is_some());
        assert!(table.column_by_name("nonexistent").is_none());
        assert_eq!(table.column_position_by_name("name"), Some(1));
    }

    #[test]
    fn test_fluent_builder_pattern() {
        // This test demonstrates the fluent builder pattern
        let table = TableDefinition::new("Extract")
            .with_schema("Extract")
            .add_required_column("Customer ID", SqlType::text())
            .add_required_column("Customer Name", SqlType::text())
            .add_required_column("Loyalty Reward Points", SqlType::big_int())
            .add_required_column("Segment", SqlType::text());

        assert_eq!(table.column_count(), 4);
        assert_eq!(table.schema, Some("Extract".to_string()));
        assert_eq!(table.name, "Extract");
    }
}
