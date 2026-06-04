// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `HyperDB` SQL dialect backend for [sea-query](https://crates.io/crates/sea-query).
//!
//! This crate provides [`HyperQueryBuilder`], a sea-query backend that generates SQL
//! compatible with `HyperDB`'s SQL dialect. `HyperDB` is largely PostgreSQL-compatible, so
//! this builder currently delegates to [`PostgresQueryBuilder`] for all operations.
//!
//! As Hyper's SQL dialect evolves and diverges from `PostgreSQL`, this builder will be
//! updated to emit Hyper-specific syntax where needed (e.g., type names, function calls,
//! or DDL extensions).
//!
//! # Usage
//!
//! `HyperQueryBuilder` implements all sea-query backend traits ([`QueryBuilder`],
//! [`SchemaBuilder`], [`GenericBuilder`]), so it can be used anywhere a sea-query
//! backend is accepted:
//!
//! ```rust
//! use sea_query::{Query, Expr, ExprTrait, Iden, PostgresQueryBuilder};
//! use sea_query_hyperdb::HyperQueryBuilder;
//!
//! #[derive(Iden)]
//! enum Users {
//!     Table,
//!     Id,
//!     Name,
//! }
//!
//! let query = Query::select()
//!     .column(Users::Id)
//!     .column(Users::Name)
//!     .from(Users::Table)
//!     .and_where(Expr::col(Users::Id).gt(10))
//!     .to_owned();
//!
//! // Build with parameter placeholders ($1, $2, ...)
//! let (sql, values) = query.build(HyperQueryBuilder);
//! assert!(sql.contains("SELECT"));
//!
//! // Or get the SQL string with values inlined
//! let sql_string = query.to_string(HyperQueryBuilder);
//! assert!(sql_string.contains("WHERE"));
//! ```
//!
//! # With hyperdb-api
//!
//! ```rust,ignore
//! use hyperdb_api::Connection;
//! use sea_query::{Query, Expr, Iden};
//! use sea_query_hyperdb::HyperQueryBuilder;
//!
//! #[derive(Iden)]
//! enum Products {
//!     Table,
//!     Name,
//!     Price,
//! }
//!
//! let query = Query::select()
//!     .column(Products::Name)
//!     .column(Products::Price)
//!     .from(Products::Table)
//!     .and_where(Expr::col(Products::Price).gt(100))
//!     .to_owned();
//!
//! let sql = query.to_string(HyperQueryBuilder);
//! let result = conn.execute_query(&sql)?;
//! ```

use sea_query::backend::{
    EscapeBuilder, ForeignKeyBuilder, GenericBuilder, IndexBuilder, OperLeftAssocDecider,
    PrecedenceDecider, QueryBuilder, QuotedBuilder, SchemaBuilder, TableBuilder, TableRefBuilder,
};
use sea_query::{
    BinOper, ColumnDef, ColumnType, ExplainStatement, Expr, ForeignKeyCreateStatement,
    ForeignKeyDropStatement, IndexCreateStatement, IndexDropStatement, Oper, PostgresQueryBuilder,
    Quote, SelectInto, SubQueryStatement, TableAlterStatement, TableRef, TableRenameStatement,
    Value,
};

/// HyperDB-specific SQL dialect backend for sea-query.
///
/// Generates SQL compatible with `HyperDB`'s dialect, as documented at
/// <https://developer.salesforce.com/docs/data/data-cloud-query-guide/references/dc-sql-reference/select.html>.
///
/// # Delegation Strategy
///
/// `HyperDB`'s SQL dialect is largely PostgreSQL-compatible — identifier quoting,
/// placeholder syntax (`$1, $2, ...`), operator precedence, and DDL/DML syntax
/// all match `PostgreSQL`. Rather than duplicating that logic, this builder
/// delegates to [`PostgresQueryBuilder`] for all operations.
///
/// As Hyper's dialect evolves (e.g., Hyper-specific type names, functions, or
/// DDL extensions), individual trait methods will be overridden to emit
/// Hyper-specific syntax while the rest continues to delegate.
///
/// Notable `HyperDB` SQL features:
/// - Optional `FROM` clause for simple expressions
/// - `DISTINCT ON` extension
/// - Standard `UNION`, `INTERSECT`, `EXCEPT` operations
/// - Zero-column result tables
///
/// # Examples
///
/// ```rust
/// use sea_query::{Query, Expr, ExprTrait, Iden};
/// use sea_query_hyperdb::HyperQueryBuilder;
///
/// #[derive(Iden)]
/// enum Users {
///     Table,
///     Name,
///     Age,
/// }
///
/// let sql = Query::select()
///     .column(Users::Name)
///     .from(Users::Table)
///     .and_where(Expr::col(Users::Age).gt(18))
///     .to_string(HyperQueryBuilder);
///
/// assert!(sql.contains(r#""users""#));
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct HyperQueryBuilder;

impl QuotedBuilder for HyperQueryBuilder {
    fn quote(&self) -> Quote {
        PostgresQueryBuilder.quote()
    }
}

impl EscapeBuilder for HyperQueryBuilder {}

impl TableRefBuilder for HyperQueryBuilder {}

impl OperLeftAssocDecider for HyperQueryBuilder {
    fn well_known_left_associative(&self, op: &BinOper) -> bool {
        PostgresQueryBuilder.well_known_left_associative(op)
    }
}

impl PrecedenceDecider for HyperQueryBuilder {
    fn inner_expr_well_known_greater_precedence(&self, inner: &Expr, outer_oper: &Oper) -> bool {
        PostgresQueryBuilder.inner_expr_well_known_greater_precedence(inner, outer_oper)
    }
}

impl QueryBuilder for HyperQueryBuilder {
    fn prepare_query_statement(
        &self,
        query: &SubQueryStatement,
        sql: &mut impl sea_query::SqlWriter,
    ) {
        PostgresQueryBuilder.prepare_query_statement(query, sql);
    }

    fn prepare_select_into(&self, into_table: &SelectInto, sql: &mut impl sea_query::SqlWriter) {
        PostgresQueryBuilder.prepare_select_into(into_table, sql);
    }

    fn prepare_explain_statement(
        &self,
        explain: &ExplainStatement,
        sql: &mut impl sea_query::SqlWriter,
    ) {
        PostgresQueryBuilder.prepare_explain_statement(explain, sql);
    }

    fn prepare_value(&self, value: Value, sql: &mut impl sea_query::SqlWriter) {
        PostgresQueryBuilder.prepare_value(value, sql);
    }

    fn placeholder(&self) -> (&'static str, bool) {
        PostgresQueryBuilder.placeholder()
    }
}

impl ForeignKeyBuilder for HyperQueryBuilder {
    fn prepare_table_ref_fk_stmt(&self, table_ref: &TableRef, sql: &mut impl sea_query::SqlWriter) {
        PostgresQueryBuilder.prepare_table_ref_fk_stmt(table_ref, sql);
    }

    fn prepare_foreign_key_create_statement_internal(
        &self,
        create: &ForeignKeyCreateStatement,
        sql: &mut impl sea_query::SqlWriter,
        mode: sea_query::backend::Mode,
    ) {
        PostgresQueryBuilder.prepare_foreign_key_create_statement_internal(create, sql, mode);
    }

    fn prepare_foreign_key_drop_statement_internal(
        &self,
        drop: &ForeignKeyDropStatement,
        sql: &mut impl sea_query::SqlWriter,
        mode: sea_query::backend::Mode,
    ) {
        PostgresQueryBuilder.prepare_foreign_key_drop_statement_internal(drop, sql, mode);
    }
}

impl IndexBuilder for HyperQueryBuilder {
    fn prepare_index_create_statement(
        &self,
        create: &IndexCreateStatement,
        sql: &mut impl sea_query::SqlWriter,
    ) {
        PostgresQueryBuilder.prepare_index_create_statement(create, sql);
    }

    fn prepare_table_ref_index_stmt(
        &self,
        table_ref: &TableRef,
        sql: &mut impl sea_query::SqlWriter,
    ) {
        PostgresQueryBuilder.prepare_table_ref_index_stmt(table_ref, sql);
    }

    fn prepare_index_drop_statement(
        &self,
        drop: &IndexDropStatement,
        sql: &mut impl sea_query::SqlWriter,
    ) {
        PostgresQueryBuilder.prepare_index_drop_statement(drop, sql);
    }

    fn prepare_index_prefix(
        &self,
        create: &IndexCreateStatement,
        sql: &mut impl sea_query::SqlWriter,
    ) {
        PostgresQueryBuilder.prepare_index_prefix(create, sql);
    }
}

impl TableBuilder for HyperQueryBuilder {
    fn prepare_column_def(&self, column_def: &ColumnDef, sql: &mut impl sea_query::SqlWriter) {
        PostgresQueryBuilder.prepare_column_def(column_def, sql);
    }

    fn prepare_column_type(&self, column_type: &ColumnType, sql: &mut impl sea_query::SqlWriter) {
        PostgresQueryBuilder.prepare_column_type(column_type, sql);
    }

    fn column_spec_auto_increment_keyword(&self) -> &str {
        PostgresQueryBuilder.column_spec_auto_increment_keyword()
    }

    fn prepare_table_alter_statement(
        &self,
        alter: &TableAlterStatement,
        sql: &mut impl sea_query::SqlWriter,
    ) {
        PostgresQueryBuilder.prepare_table_alter_statement(alter, sql);
    }

    fn prepare_table_rename_statement(
        &self,
        rename: &TableRenameStatement,
        sql: &mut impl sea_query::SqlWriter,
    ) {
        PostgresQueryBuilder.prepare_table_rename_statement(rename, sql);
    }
}

impl SchemaBuilder for HyperQueryBuilder {}

impl GenericBuilder for HyperQueryBuilder {}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::{ExprTrait, Iden, Query};

    #[derive(Iden)]
    enum Users {
        Table,
        Id,
        Name,
        Age,
    }

    #[test]
    fn select_generates_valid_sql() {
        let query = Query::select()
            .column(Users::Name)
            .from(Users::Table)
            .and_where(Expr::col(Users::Age).gt(18))
            .to_owned();

        let sql = query.to_string(HyperQueryBuilder);
        assert_eq!(
            sql,
            query.to_string(PostgresQueryBuilder),
            "HyperQueryBuilder should produce identical SQL to PostgresQueryBuilder"
        );
    }

    #[test]
    fn select_build_produces_placeholders() {
        let query = Query::select()
            .column(Users::Name)
            .from(Users::Table)
            .and_where(Expr::col(Users::Age).gt(18))
            .to_owned();

        let (sql, values) = query.build(HyperQueryBuilder);
        let (pg_sql, pg_values) = query.build(PostgresQueryBuilder);
        assert_eq!(sql, pg_sql);
        assert_eq!(format!("{values:?}"), format!("{pg_values:?}"));
    }

    #[test]
    fn insert_generates_valid_sql() {
        let query = Query::insert()
            .into_table(Users::Table)
            .columns([Users::Name, Users::Age])
            .values_panic(["Alice".into(), 30i32.into()])
            .to_owned();

        let sql = query.to_string(HyperQueryBuilder);
        assert_eq!(sql, query.to_string(PostgresQueryBuilder));
    }

    #[test]
    fn update_generates_valid_sql() {
        let query = Query::update()
            .table(Users::Table)
            .value(Users::Name, "Bob")
            .and_where(Expr::col(Users::Id).eq(1))
            .to_owned();

        let sql = query.to_string(HyperQueryBuilder);
        assert_eq!(sql, query.to_string(PostgresQueryBuilder));
    }

    #[test]
    fn delete_generates_valid_sql() {
        let query = Query::delete()
            .from_table(Users::Table)
            .and_where(Expr::col(Users::Id).eq(1))
            .to_owned();

        let sql = query.to_string(HyperQueryBuilder);
        assert_eq!(sql, query.to_string(PostgresQueryBuilder));
    }

    #[test]
    fn create_table_generates_valid_sql() {
        use sea_query::{ColumnDef, Table};

        let stmt = Table::create()
            .table(Users::Table)
            .if_not_exists()
            .col(ColumnDef::new(Users::Id).integer().not_null().primary_key())
            .col(ColumnDef::new(Users::Name).string().not_null())
            .col(ColumnDef::new(Users::Age).integer())
            .to_owned();

        let sql = stmt.to_string(HyperQueryBuilder);
        assert_eq!(sql, stmt.to_string(PostgresQueryBuilder));
    }

    #[test]
    fn default_trait() {
        let builder = HyperQueryBuilder;
        // Touch `builder` so the test body demonstrates the unit-struct is constructible.
        let _ = builder;
    }
}
