// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Database catalog operations.
//!
//! The `Catalog` struct provides methods for working with database metadata,
//! including creating and dropping databases, schemas, and tables.
//!
//! # SQL Injection Prevention
//!
//! All catalog methods use SQL identifier and literal escaping to prevent
//! SQL injection attacks:
//!
//! - Identifiers (database names, schema names, table names) are quoted with
//!   double quotes and internal quotes are escaped (e.g., `"` → `""`)
//! - String literals (comparison values) are quoted with single quotes and
//!   internal quotes are escaped (e.g., `'` → `''`)
//!
//! While this provides protection against basic SQL injection, parameterized
//! queries would be more robust. The escaping methods used are:
//!
//! - `name.replace('"', "\"\"")` for identifiers
//! - `value.replace('\'', "''")` for literals
//!
//! **Note**: User-provided names should still be validated against expected
//! patterns when possible, as a defense-in-depth measure.

use crate::connection::Connection;
use crate::error::{Error, Result};
use crate::table_definition::TableDefinition;
use hyperdb_api_core::types::SqlType;

/// Provides catalog operations for database metadata.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{Connection, Catalog, CreateMode, Result};
///
/// fn main() -> Result<()> {
///     let conn = Connection::connect("localhost:7483", "example.hyper", CreateMode::CreateIfNotExists)?;
///     let catalog = Catalog::new(&conn);
///
///     // Check if a schema exists
///     if !catalog.has_schema("my_schema")? {
///         catalog.create_schema("my_schema")?;
///     }
///
///     // List tables
///     let tables = catalog.get_table_names("my_schema")?;
///     for table in tables {
///         println!("Table: {}", table);
///     }
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct Catalog<'conn> {
    connection: &'conn Connection,
}

impl<'conn> Catalog<'conn> {
    /// Creates a new Catalog for the given connection.
    pub fn new(connection: &'conn Connection) -> Self {
        Catalog { connection }
    }

    // ============================================================
    // Database Operations
    // ============================================================

    /// Creates a new database file (delegates to Connection).
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::create_database`].
    pub fn create_database(&self, path: &str) -> Result<()> {
        self.connection.create_database(path)
    }

    /// Drops (deletes) a database file (delegates to Connection).
    ///
    /// # Errors
    ///
    /// Forwards the error from [`Connection::drop_database`].
    pub fn drop_database(&self, path: &str) -> Result<()> {
        self.connection.drop_database(path)
    }

    /// Attaches a database file to the connection.
    ///
    /// Once attached, the database can be queried and modified.
    /// The database is identified by its alias (or by its path if no alias is provided).
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the database file to attach.
    /// * `alias` - Optional alias for the database. If `None`, the database is
    ///   attached without an explicit alias (typically using its filename).
    ///
    /// # Errors
    ///
    /// Returns an error if the database file doesn't exist or if attachment fails.
    pub fn attach_database(&self, path: &str, alias: Option<&str>) -> Result<()> {
        self.connection.attach_database(path, alias)
    }

    /// Detaches a database from the connection.
    ///
    /// After detaching, the database file is released and can be accessed
    /// externally (e.g., copied, moved, etc.). All pending updates are
    /// written to disk before detaching.
    ///
    /// # Arguments
    ///
    /// * `alias` - The alias of the database to detach.
    ///
    /// # Errors
    ///
    /// Returns an error if the database is not attached or if detachment fails.
    pub fn detach_database(&self, alias: &str) -> Result<()> {
        self.connection.detach_database(alias)
    }

    /// Detaches all databases from the connection.
    ///
    /// This is useful for cleanup before closing a connection or when
    /// you need to release all database files.
    ///
    /// # Errors
    ///
    /// Returns an error if the databases could not be detached.
    pub fn detach_all_databases(&self) -> Result<()> {
        self.connection.detach_all_databases()
    }

    // ============================================================
    // Schema Operations
    // ============================================================

    /// Creates a schema.
    ///
    /// # Errors
    ///
    /// - Returns an error if `schema_name` cannot be converted to a
    ///   [`SchemaName`](crate::SchemaName).
    /// - Returns [`Error::Server`] if the server rejects
    ///   `CREATE SCHEMA IF NOT EXISTS`.
    pub fn create_schema<T>(&self, schema_name: T) -> Result<()>
    where
        T: TryInto<crate::SchemaName>,
        crate::Error: From<T::Error>,
    {
        let schema = schema_name.try_into()?;
        let sql = format!("CREATE SCHEMA IF NOT EXISTS {schema}");
        self.connection.execute_command(&sql)?;
        Ok(())
    }

    // ============================================================
    // Query Operations
    // ============================================================

    /// Returns a list of schema names in the database.
    ///
    /// # Arguments
    ///
    /// * `database` - The database name, or `None` to use the first database
    ///   in the search path.
    ///
    /// # Returns
    ///
    /// A vector of schema names.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub fn get_schema_names<T>(&self, database: Option<T>) -> Result<Vec<String>>
    where
        T: TryInto<crate::DatabaseName>,
        crate::Error: From<T::Error>,
    {
        let database = match database {
            Some(db) => Some(db.try_into()?),
            None => None,
        };

        let query = if let Some(db) = database {
            format!(
                "SELECT nspname FROM {db}.pg_catalog.pg_namespace WHERE nspname NOT IN ('pg_catalog', 'pg_temp', 'information_schema')"
            )
        } else {
            "SELECT nspname FROM pg_catalog.pg_namespace WHERE nspname NOT IN ('pg_catalog', 'pg_temp', 'information_schema')".to_string()
        };

        let mut result = self.connection.execute_query(&query)?;
        let mut names = Vec::new();
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                if let Some(name) = row.get::<String>(0) {
                    names.push(name);
                }
            }
        }
        Ok(names)
    }

    /// Returns a list of table names in the given schema.
    ///
    /// # Arguments
    ///
    /// * `schema` - The schema name (can include database qualifier).
    ///
    /// # Returns
    ///
    /// A vector of table names.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub fn get_table_names<T>(&self, schema: T) -> Result<Vec<String>>
    where
        T: TryInto<crate::SchemaName>,
        crate::Error: From<T::Error>,
    {
        let schema = schema.try_into()?;
        let db_prefix = if let Some(db) = schema.database() {
            format!("{db}.")
        } else {
            String::new()
        };

        let query = format!(
            "SELECT tablename FROM {}pg_catalog.pg_tables WHERE schemaname = '{}'",
            db_prefix,
            schema.unescaped().replace('\'', "''")
        );

        let mut result = self.connection.execute_query(&query)?;
        let mut names = Vec::new();
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                if let Some(name) = row.get::<String>(0) {
                    names.push(name);
                }
            }
        }
        Ok(names)
    }

    /// Checks whether a schema exists.
    ///
    /// # Arguments
    ///
    /// * `schema` - The schema name (can include database qualifier).
    ///
    /// # Returns
    ///
    /// `true` if the schema exists, `false` otherwise.
    ///
    /// # Errors
    ///
    /// - Returns an error if `schema` cannot be converted to a
    ///   [`SchemaName`](crate::SchemaName).
    /// - Returns [`Error::Server`] if the `pg_catalog.pg_namespace` lookup
    ///   query fails.
    pub fn has_schema<T>(&self, schema: T) -> Result<bool>
    where
        T: TryInto<crate::SchemaName>,
        crate::Error: From<T::Error>,
    {
        let schema = schema.try_into()?;
        let db_prefix = if let Some(db) = schema.database() {
            format!("{db}.")
        } else {
            String::new()
        };

        let query = format!(
            "SELECT 1 FROM {}pg_catalog.pg_namespace WHERE nspname = '{}'",
            db_prefix,
            schema.unescaped().replace('\'', "''")
        );

        let mut result = self.connection.execute_query(&query)?;
        if let Some(chunk) = result.next_chunk()? {
            Ok(!chunk.is_empty())
        } else {
            Ok(false)
        }
    }

    /// Checks whether a table exists.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The table name (can include database and schema qualifiers).
    ///
    /// # Returns
    ///
    /// `true` if the table exists, `false` otherwise.
    ///
    /// # Errors
    ///
    /// - Returns an error if `table_name` cannot be converted to a
    ///   [`TableName`](crate::TableName).
    /// - Returns [`Error::Server`] if the `pg_catalog.pg_tables` lookup
    ///   query fails.
    pub fn has_table<T>(&self, table_name: T) -> Result<bool>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let table_name = table_name.try_into()?;
        let schema = table_name
            .schema()
            .map_or("public", super::names::Name::unescaped);
        let db_prefix = if let Some(db) = table_name.database() {
            format!("{db}.")
        } else {
            String::new()
        };

        let query = format!(
            "SELECT 1 FROM {}pg_catalog.pg_tables WHERE schemaname = '{}' AND tablename = '{}'",
            db_prefix,
            schema.replace('\'', "''"),
            table_name.table().unescaped().replace('\'', "''")
        );

        let mut result = self.connection.execute_query(&query)?;
        if let Some(chunk) = result.next_chunk()? {
            Ok(!chunk.is_empty())
        } else {
            Ok(false)
        }
    }

    /// Retrieves the table definition for an existing table.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The table name (can include database and schema qualifiers).
    ///
    /// # Returns
    ///
    /// A [`TableDefinition`] representing the table's schema.
    ///
    /// # Errors
    ///
    /// Returns an error if the table does not exist or if retrieval fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Connection, Catalog, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::without_database("localhost:7483")?;
    ///     let catalog = Catalog::new(&conn);
    ///
    ///     let table_def = catalog.get_table_definition("public.products")?;
    ///     println!("Columns: {}", table_def.column_count());
    ///     for col in table_def.columns() {
    ///         println!("  - {}: {}", col.name, col.type_name());
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub fn get_table_definition<T>(&self, table_name: T) -> Result<TableDefinition>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let table_name = table_name.try_into()?;
        let schema = table_name
            .schema()
            .map_or("public", super::names::Name::unescaped);
        let table = table_name.table().unescaped();

        // Query column information from pg_catalog
        // Join pg_attribute with pg_type to get column names, types, and type modifiers
        let query = if let Some(db) = table_name.database() {
            format!(
                r"SELECT a.attname, t.typname, NOT a.attnotnull as is_nullable, a.atttypid, a.atttypmod
                 FROM {db}.pg_catalog.pg_attribute a
                 JOIN {db}.pg_catalog.pg_type t ON a.atttypid = t.oid
                 JOIN {db}.pg_catalog.pg_class c ON a.attrelid = c.oid
                 JOIN {db}.pg_catalog.pg_namespace n ON c.relnamespace = n.oid
                 WHERE n.nspname = '{schema}' AND c.relname = '{table}'
                   AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY a.attnum",
                db = db,
                schema = schema.replace('\'', "''"),
                table = table.replace('\'', "''")
            )
        } else {
            format!(
                r"SELECT a.attname, t.typname, NOT a.attnotnull as is_nullable, a.atttypid, a.atttypmod
                 FROM pg_catalog.pg_attribute a
                 JOIN pg_catalog.pg_type t ON a.atttypid = t.oid
                 JOIN pg_catalog.pg_class c ON a.attrelid = c.oid
                 JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid
                 WHERE n.nspname = '{schema}' AND c.relname = '{table}'
                   AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY a.attnum",
                schema = schema.replace('\'', "''"),
                table = table.replace('\'', "''")
            )
        };

        let mut result = self.connection.execute_query(&query)?;

        let mut table_def = TableDefinition::new(table);
        table_def.schema = Some(schema.to_string());
        if let Some(db) = table_name.database() {
            table_def.database = Some(db.unescaped().to_string());
        }

        let mut found_columns = false;
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                found_columns = true;
                let col_name = row.get::<String>(0).unwrap_or_default();
                let _data_type = row.get::<String>(1).unwrap_or_default();
                // Hyper returns boolean as binary bool
                let is_nullable = row.get::<bool>(2).unwrap_or(false);

                // Get type OID and modifier for proper type construction.
                // Bit-pattern reinterpret: pg_type.oid is transported as Int4 on the
                // wire but semantically is a u32 OID; this `as u32` recovers the
                // original bit pattern.
                #[expect(
                    clippy::cast_sign_loss,
                    reason = "intentional u32 bit-pattern reinterpret of PostgreSQL oid transported as Int4"
                )]
                let type_oid = row.get::<i32>(3).unwrap_or(0) as u32;
                let type_mod = row.get::<i32>(4).unwrap_or(-1);

                // Use OID and modifier to create proper SqlType with precision/scale
                let sql_type = SqlType::from_oid_and_modifier(type_oid, type_mod);
                table_def.add_column_with_sql_type(&col_name, sql_type, is_nullable);
            }
        }

        if !found_columns {
            return Err(Error::not_found(format!("Table {schema}.{table}")));
        }

        Ok(table_def)
    }

    // ============================================================
    // Table Operations
    // ============================================================

    /// Creates a table from a definition.
    ///
    /// # Arguments
    ///
    /// * `table_def` - The table definition describing the table to create.
    ///
    /// # Errors
    ///
    /// Returns an error if the table already exists or if creation fails.
    pub fn create_table(&self, table_def: &TableDefinition) -> Result<()> {
        let sql = table_def.to_create_sql(true)?;
        self.connection.execute_command(&sql)?;
        Ok(())
    }

    /// Creates a table from a definition if it doesn't exist.
    ///
    /// Unlike [`create_table`](Self::create_table), this method does not fail
    /// if the table already exists.
    ///
    /// # Errors
    ///
    /// - Returns [`Error::InvalidTableDefinition`] if `table_def` cannot be
    ///   rendered as valid SQL (zero columns, bad identifiers).
    /// - Returns [`Error::Server`] if the server rejects
    ///   `CREATE TABLE IF NOT EXISTS`.
    pub fn create_table_if_not_exists(&self, table_def: &TableDefinition) -> Result<()> {
        let sql = table_def.to_create_sql(false)?;
        self.connection.execute_command(&sql)?;
        Ok(())
    }

    /// Drops a table.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The table name (can include database and schema qualifiers).
    ///
    /// # Errors
    ///
    /// Returns an error if the table doesn't exist or if deletion fails.
    pub fn drop_table<T>(&self, table_name: T) -> Result<()>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let table_name = table_name.try_into()?;
        let sql = format!("DROP TABLE {table_name}");
        self.connection.execute_command(&sql)?;
        Ok(())
    }

    /// Drops a table if it exists.
    ///
    /// Unlike [`drop_table`](Self::drop_table), this method does not fail
    /// if the table doesn't exist.
    ///
    /// # Errors
    ///
    /// - Returns an error if `table_name` cannot be converted to a
    ///   [`TableName`](crate::TableName).
    /// - Returns [`Error::Server`] if the server rejects
    ///   `DROP TABLE IF EXISTS`.
    pub fn drop_table_if_exists<T>(&self, table_name: T) -> Result<()>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let table_name = table_name.try_into()?;
        let sql = format!("DROP TABLE IF EXISTS {table_name}");
        self.connection.execute_command(&sql)?;
        Ok(())
    }

    /// Drops a schema.
    ///
    /// # Arguments
    ///
    /// * `schema_name` - The schema name (can include database qualifier).
    /// * `cascade` - If true, drop all objects in the schema.
    ///
    /// # Errors
    ///
    /// Returns an error if the schema doesn't exist or if deletion fails.
    pub fn drop_schema<T>(&self, schema_name: T, cascade: bool) -> Result<()>
    where
        T: TryInto<crate::SchemaName>,
        crate::Error: From<T::Error>,
    {
        let schema_name = schema_name.try_into()?;
        let sql = if cascade {
            format!("DROP SCHEMA {schema_name} CASCADE")
        } else {
            format!("DROP SCHEMA {schema_name}")
        };
        self.connection.execute_command(&sql)?;
        Ok(())
    }

    /// Drops a schema if it exists.
    ///
    /// # Errors
    ///
    /// - Returns an error if `schema_name` cannot be converted to a
    ///   [`SchemaName`](crate::SchemaName).
    /// - Returns [`Error::Server`] if the server rejects
    ///   `DROP SCHEMA IF EXISTS` — typically because `cascade` was `false`
    ///   and the schema is not empty.
    pub fn drop_schema_if_exists<T>(&self, schema_name: T, cascade: bool) -> Result<()>
    where
        T: TryInto<crate::SchemaName>,
        crate::Error: From<T::Error>,
    {
        let schema_name = schema_name.try_into()?;
        let sql = if cascade {
            format!("DROP SCHEMA IF EXISTS {schema_name} CASCADE")
        } else {
            format!("DROP SCHEMA IF EXISTS {schema_name}")
        };
        self.connection.execute_command(&sql)?;
        Ok(())
    }

    // ============================================================
    // Metadata Helpers
    // ============================================================

    /// Returns the approximate row count for a table.
    ///
    /// This executes `SELECT COUNT(*) FROM table_name`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Catalog, CreateMode, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let catalog = Catalog::new(&conn);
    /// let count = catalog.get_row_count("public.users")?;
    /// println!("Users: {}", count);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns an error if `table_name` cannot be converted to a
    ///   [`TableName`](crate::TableName).
    /// - Returns [`Error::Server`] if the `SELECT COUNT(*)` query fails
    ///   (e.g. table does not exist).
    pub fn get_row_count<T>(&self, table_name: T) -> Result<i64>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let table_name = table_name.try_into()?;
        self.connection
            .query_count(&format!("SELECT COUNT(*) FROM {table_name}"))
    }

    /// Returns the column names for a table.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Catalog, CreateMode, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let catalog = Catalog::new(&conn);
    /// let columns = catalog.get_column_names("public.users")?;
    /// for col in &columns {
    ///     println!("Column: {}", col);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Forwards the error from
    /// [`get_table_definition`](Self::get_table_definition) — invalid
    /// `table_name`, missing table, or a failed catalog query.
    pub fn get_column_names<T>(&self, table_name: T) -> Result<Vec<String>>
    where
        T: TryInto<crate::TableName>,
        crate::Error: From<T::Error>,
    {
        let table_def = self.get_table_definition(table_name)?;
        Ok(table_def.columns().iter().map(|c| c.name.clone()).collect())
    }

    /// Returns a list of attached database names.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, Catalog, CreateMode, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let catalog = Catalog::new(&conn);
    /// let databases = catalog.get_database_names()?;
    /// for db in &databases {
    ///     println!("Database: {}", db);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Server`] if the
    /// `SELECT datname FROM pg_catalog.pg_database` query fails or a
    /// streaming error occurs while draining the result.
    pub fn get_database_names(&self) -> Result<Vec<String>> {
        let query = "SELECT datname FROM pg_catalog.pg_database";
        let mut result = self.connection.execute_query(query)?;
        let mut names = Vec::new();
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                if let Some(name) = row.get::<String>(0) {
                    names.push(name);
                }
            }
        }
        Ok(names)
    }
}
