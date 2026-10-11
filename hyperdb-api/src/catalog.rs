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
use crate::table_copy::{CopyTableReport, UnpreservedItem, UnpreservedReason};
use crate::table_definition::{TableConstraint, TableDefinition};
use hyperdb_api_core::protocol::escape::QuotedIdentifier;
use hyperdb_api_core::types::SqlType;

/// The collation every uncollated column reports in `pg_collation.collname`.
///
/// It is a read-only sentinel: the engine rejects `COLLATE "default"` with
/// `unknown collation "default"`, so it must never be echoed back into DDL.
const DEFAULT_COLLATION: &str = "default";

/// One `pg_constraint` row group being accumulated across its column rows.
struct PendingConstraint {
    contype: String,
    conname: String,
    validated: bool,
    columns: Vec<String>,
}

/// The outcome of reflecting a table's constraints.
#[derive(Default)]
struct ReflectedConstraints {
    /// Constraints a `CREATE TABLE` can restate exactly.
    supported: Vec<TableConstraint>,
    /// Constraints that cannot be reproduced, as `(description, columns)`.
    unreproducible: Vec<(String, Vec<String>)>,
}

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
    /// The returned definition carries the full schema Hyper is able to
    /// record: column names and types, `NOT NULL`, `DEFAULT` expressions, and
    /// the assumed key constraints
    /// ([`TableConstraint`](crate::TableConstraint)). Real `PRIMARY KEY`,
    /// `UNIQUE`, `FOREIGN KEY`, and `CHECK` constraints are rejected by the
    /// engine at `CREATE TABLE`, so a Hyper table never carries one.
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

        // Query column information from pg_catalog. `pg_attrdef` and
        // `pg_collation` are joined rather than queried separately so the
        // DEFAULT expressions and collations arrive in the same round trip,
        // already lined up with their columns.
        //
        // `db` is already an escaped identifier; `schema`/`table` are compared
        // as string literals, so they get single-quote doubling instead.
        let catalog_prefix = table_name
            .database()
            .map_or_else(|| "pg_catalog".to_string(), |db| format!("{db}.pg_catalog"));
        let query = format!(
            r"SELECT a.attname, t.typname, NOT a.attnotnull as is_nullable, a.atttypid, a.atttypmod, ad.adsrc, coll.collname
                 FROM {cat}.pg_attribute a
                 JOIN {cat}.pg_type t ON a.atttypid = t.oid
                 JOIN {cat}.pg_class c ON a.attrelid = c.oid
                 JOIN {cat}.pg_namespace n ON c.relnamespace = n.oid
                 LEFT JOIN {cat}.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
                 LEFT JOIN {cat}.pg_collation coll ON coll.oid = a.attcollation
                 WHERE n.nspname = '{schema}' AND c.relname = '{table}'
                   AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY a.attnum",
            cat = catalog_prefix,
            schema = schema.replace('\'', "''"),
            table = table.replace('\'', "''")
        );

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

                if let Some(default_expr) = row.get::<String>(5)
                    && let Some(column) = table_def.columns.last_mut()
                {
                    column.set_default_expr(default_expr);
                }

                // Every column reports a collation; an uncollated one reports
                // the sentinel `default`, which the engine refuses to accept
                // back (`unknown collation "default"`). Only a real, named
                // collation is worth recording.
                if let Some(collation) = row
                    .get::<String>(6)
                    .filter(|name| name != DEFAULT_COLLATION)
                    && let Some(column) = table_def.columns.last_mut()
                {
                    column.set_collation(collation);
                }
            }
        }

        if !found_columns {
            return Err(Error::not_found(format!("Table {schema}.{table}")));
        }

        table_def.set_constraints(self.get_table_constraints(&table_name)?.supported);

        Ok(table_def)
    }

    /// Reads the key constraints declared on a table.
    ///
    /// `conkey` holds the constrained columns as an array of `attnum`s;
    /// `unnest … WITH ORDINALITY` turns it into ordered column names, which is
    /// what `CREATE TABLE` needs. Constraint *names* are not read back because
    /// Hyper rejects `CONSTRAINT <name> …` on `CREATE TABLE` (`named
    /// constraints not implemented yet`) — the engine derives its own.
    ///
    /// `convalidated` separates the two things a `contype` of `p` can mean. An
    /// `ASSUMED PRIMARY KEY` — the only kind this engine build will accept —
    /// reads back as `convalidated = false`. A `.hyper` written by an engine
    /// with index support would carry an *enforced* key instead, which cannot
    /// be reproduced here and must not be quietly re-emitted as `ASSUMED`:
    /// that would downgrade an enforced constraint to an unenforced one and
    /// report it as preserved. Anything that is not a known-assumed key is
    /// therefore returned as unreproducible rather than mapped.
    fn get_table_constraints(&self, table_name: &crate::TableName) -> Result<ReflectedConstraints> {
        let schema = table_name
            .schema()
            .map_or("public", super::names::Name::unescaped);
        let table = table_name.table().unescaped();
        let catalog_prefix = table_name
            .database()
            .map_or_else(|| "pg_catalog".to_string(), |db| format!("{db}.pg_catalog"));

        let query = format!(
            r"SELECT con.conname, CAST(con.contype AS TEXT) AS contype, a.attname, con.convalidated
                 FROM {cat}.pg_constraint con
                 JOIN {cat}.pg_class c ON con.conrelid = c.oid
                 JOIN {cat}.pg_namespace n ON c.relnamespace = n.oid,
                 unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
                 JOIN {cat}.pg_attribute a
                   ON a.attrelid = con.conrelid AND a.attnum = k.attnum
                 WHERE n.nspname = '{schema}' AND c.relname = '{table}'
                 ORDER BY con.contype, con.conname, k.ord",
            cat = catalog_prefix,
            schema = schema.replace('\'', "''"),
            table = table.replace('\'', "''")
        );

        // Grouped by (contype, conname) in the ORDER BY above, so consecutive
        // rows with the same key belong to the same constraint. A primary key
        // has an empty `conname`, which is why the type is part of the key.
        let mut reflected = ReflectedConstraints::default();
        let mut current: Option<PendingConstraint> = None;

        let mut result = self.connection.execute_query(&query)?;
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                let conname = row.get::<String>(0).unwrap_or_default();
                let contype = row.get::<String>(1).unwrap_or_default();
                let attname = row.get::<String>(2).unwrap_or_default();
                let validated = row.get::<bool>(3).unwrap_or(false);

                match &mut current {
                    Some(pending) if pending.contype == contype && pending.conname == conname => {
                        pending.columns.push(attname);
                    }
                    slot => {
                        if let Some(finished) = slot.take() {
                            Self::push_constraint(&mut reflected, finished);
                        }
                        *slot = Some(PendingConstraint {
                            contype,
                            conname,
                            validated,
                            columns: vec![attname],
                        });
                    }
                }
            }
        }
        if let Some(finished) = current.take() {
            Self::push_constraint(&mut reflected, finished);
        }

        Ok(reflected)
    }

    /// Maps one reflected `pg_constraint` row group onto a [`TableConstraint`],
    /// or records it as unreproducible.
    ///
    /// Only an unvalidated `p` or `u` is an assumed key that `CREATE TABLE`
    /// can restate. Everything else — an enforced key, a `CHECK`, a foreign
    /// key, an unknown code from a future engine — is reported rather than
    /// approximated. See [`get_table_constraints`](Self::get_table_constraints).
    fn push_constraint(out: &mut ReflectedConstraints, pending: PendingConstraint) {
        let PendingConstraint {
            contype,
            validated,
            columns,
            ..
        } = pending;

        match (contype.as_str(), validated) {
            ("p", false) => out
                .supported
                .push(TableConstraint::AssumedPrimaryKey { columns }),
            ("u", false) => out
                .supported
                .push(TableConstraint::AssumedUnique { columns }),
            (code, _) => {
                let description = match code {
                    "p" => "enforced PRIMARY KEY".to_string(),
                    "u" => "enforced UNIQUE".to_string(),
                    "c" => "CHECK".to_string(),
                    "f" => "FOREIGN KEY".to_string(),
                    "x" => "EXCLUSION".to_string(),
                    other => format!("constraint of type '{other}'"),
                };
                out.unreproducible.push((description, columns));
            }
        }
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

    /// Copies a table, reproducing its schema instead of inferring it.
    ///
    /// `CREATE TABLE … AS SELECT` derives the destination schema from the
    /// query's result columns, which carry types but no constraints, so every
    /// column of a CTAS copy comes out nullable with no defaults and no keys.
    /// This method reflects the source schema out of `pg_catalog`, issues an
    /// explicit `CREATE TABLE`, and only then moves the rows with `INSERT …
    /// SELECT`.
    ///
    /// Source and destination may live in different databases; qualify the
    /// names and both sides are addressed directly. Note that once a second
    /// database is attached to the session, Hyper can no longer resolve
    /// *unqualified* DDL (`create statement could not resolve the schema`), so
    /// cross-database callers should qualify both names fully.
    ///
    /// # Fidelity
    ///
    /// `NOT NULL`, `DEFAULT`, `COLLATE`, `ASSUMED PRIMARY KEY`, and `ASSUMED
    /// UNIQUE` are carried across. Enforced `PRIMARY KEY`, `UNIQUE`, `FOREIGN
    /// KEY`, and `CHECK` cannot be: Hyper rejects all four at `CREATE TABLE`,
    /// so no table this engine wrote has one to begin with. Should one turn up
    /// anyway — in a `.hyper` written by an engine with index support — it is
    /// reported as unpreserved rather than downgraded to its `ASSUMED` form,
    /// which would swap an enforced constraint for an unenforced one and call
    /// it preserved.
    ///
    /// Defaults are the one class that can be partly lost. Hyper stores
    /// non-literal defaults database-qualified — `NOW()` reads back as
    /// `"mydb"."pg_catalog"."now"()` — and copying that text verbatim into
    /// another database would leave the copy depending on `"mydb"` being
    /// attached. Such defaults are dropped and listed in
    /// [`CopyTableReport::unpreserved`] rather than reproduced unsoundly.
    /// **A successful return does not imply full fidelity** — check
    /// [`CopyTableReport::is_fully_preserved`].
    ///
    /// # Arguments
    ///
    /// * `source` - The table to copy from (may include database/schema qualifiers).
    /// * `destination` - The table to create (may include database/schema qualifiers).
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] if `source` does not exist.
    /// - [`Error::Server`] if the destination already exists.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api::{Catalog, Connection, Result};
    ///
    /// fn main() -> Result<()> {
    ///     let conn = Connection::without_database("localhost:7483")?;
    ///     let catalog = Catalog::new(&conn);
    ///
    ///     let report = catalog.copy_table("public.orders", "backup.public.orders")?;
    ///     println!("copied {} rows", report.rows_copied);
    ///     for item in &report.unpreserved {
    ///         eprintln!("not preserved - {item}");
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub fn copy_table<S, D>(&self, source: S, destination: D) -> Result<CopyTableReport>
    where
        S: TryInto<crate::TableName>,
        crate::Error: From<S::Error>,
        D: TryInto<crate::TableName>,
        crate::Error: From<D::Error>,
    {
        let source = source.try_into()?;
        let destination = destination.try_into()?;

        let mut table_def = self.get_table_definition(source.clone())?;
        let mut report = CopyTableReport::default();

        // Every unpreserved item is stamped with its origin: whole-database
        // copies merge one report per table, and a bare column name would not
        // say which table lost it.
        let origin = format!(
            "{}.{}",
            source
                .schema()
                .map_or("public", super::names::Name::unescaped),
            source.table().unescaped()
        );

        // Re-read the constraints for their unreproducible half, which
        // `get_table_definition` has no field to carry. One extra catalog
        // query per table is not worth widening that method's return type
        // for, next to the row copy that follows.
        for (description, columns) in self.get_table_constraints(&source)?.unreproducible {
            report.unpreserved.push(UnpreservedItem {
                table: origin.clone(),
                column: String::new(),
                reason: UnpreservedReason::UnsupportedConstraint,
                detail: format!("{description} ({})", columns.join(", ")),
            });
        }

        // Retarget the reflected definition at the destination. `schema` and
        // `database` are overwritten unconditionally so a destination that
        // omits them lands in the default location rather than inheriting the
        // source's.
        table_def.name = destination.table().unescaped().to_string();
        table_def.schema = destination.schema().map(|s| s.unescaped().to_string());
        table_def.database = destination.database().map(|d| d.unescaped().to_string());

        for column in &mut table_def.columns {
            if !column.nullable {
                report.not_null_columns = report.not_null_columns.saturating_add(1);
            }
            if column.collation().is_some() {
                report.collated_columns = report.collated_columns.saturating_add(1);
            }
            match column.default_expr() {
                Some(expr) if crate::table_copy::is_portable_default(expr) => {
                    report.default_columns = report.default_columns.saturating_add(1);
                }
                Some(expr) => {
                    report.unpreserved.push(UnpreservedItem {
                        table: origin.clone(),
                        column: column.name.clone(),
                        reason: UnpreservedReason::NonPortableDefault,
                        detail: expr.to_string(),
                    });
                    column.clear_default_expr();
                }
                None => {}
            }
        }

        for constraint in table_def.constraints() {
            match constraint {
                TableConstraint::AssumedPrimaryKey { .. } => {
                    report.assumed_primary_keys = report.assumed_primary_keys.saturating_add(1);
                }
                TableConstraint::AssumedUnique { .. } => {
                    report.assumed_unique_constraints =
                        report.assumed_unique_constraints.saturating_add(1);
                }
            }
        }

        self.create_table(&table_def)?;

        // Both column lists are spelled out so the copy does not depend on the
        // destination happening to share the source's column order. Names are
        // quoted unconditionally: a reflected name may be a reserved word,
        // which `SqlIdentifier` would emit bare.
        let columns = table_def
            .columns
            .iter()
            .map(|c| QuotedIdentifier(&c.name).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        report.rows_copied = self.connection.execute_command(&format!(
            "INSERT INTO {destination} ({columns}) SELECT {columns} FROM {source}"
        ))?;

        Ok(report)
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

    /// Returns the row count of a table.
    ///
    /// This executes `SELECT COUNT(*) FROM table_name`, so the count is exact.
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
