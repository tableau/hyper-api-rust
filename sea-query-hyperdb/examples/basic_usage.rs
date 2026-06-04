// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Demonstrates building SQL queries with `HyperQueryBuilder`.
//!
//! Run with: `cargo run -p sea-query-hyper --example basic_usage`

use sea_query::{ColumnDef, Expr, ExprTrait, Iden, Query, Table};
use sea_query_hyperdb::HyperQueryBuilder;

#[derive(Iden)]
enum Products {
    Table,
    Id,
    Name,
    Price,
    Category,
}

fn main() {
    println!("=== HyperQueryBuilder — SQL Generation Examples ===\n");

    // --- SELECT ---
    let select = Query::select()
        .columns([Products::Id, Products::Name, Products::Price])
        .from(Products::Table)
        .and_where(Expr::col(Products::Price).gt(100))
        .and_where(Expr::col(Products::Category).eq("electronics"))
        .order_by(Products::Price, sea_query::Order::Desc)
        .limit(10)
        .to_owned();

    println!("SELECT (inline values):");
    println!("  {}\n", select.to_string(HyperQueryBuilder));

    let (sql, values) = select.build(HyperQueryBuilder);
    println!("SELECT (parameterized):");
    println!("  SQL:    {sql}");
    println!("  Params: {values:?}\n");

    // --- INSERT ---
    let insert = Query::insert()
        .into_table(Products::Table)
        .columns([Products::Name, Products::Price, Products::Category])
        .values_panic(["Widget".into(), 29.99f64.into(), "gadgets".into()])
        .values_panic(["Gizmo".into(), 49.99f64.into(), "gadgets".into()])
        .to_owned();

    println!("INSERT:");
    println!("  {}\n", insert.to_string(HyperQueryBuilder));

    // --- UPDATE ---
    let update = Query::update()
        .table(Products::Table)
        .value(Products::Price, 19.99f64)
        .and_where(Expr::col(Products::Name).eq("Widget"))
        .to_owned();

    println!("UPDATE:");
    println!("  {}\n", update.to_string(HyperQueryBuilder));

    // --- DELETE ---
    let delete = Query::delete()
        .from_table(Products::Table)
        .and_where(Expr::col(Products::Id).eq(42))
        .to_owned();

    println!("DELETE:");
    println!("  {}\n", delete.to_string(HyperQueryBuilder));

    // --- CREATE TABLE ---
    let create = Table::create()
        .table(Products::Table)
        .if_not_exists()
        .col(
            ColumnDef::new(Products::Id)
                .integer()
                .not_null()
                .auto_increment()
                .primary_key(),
        )
        .col(ColumnDef::new(Products::Name).string().not_null())
        .col(ColumnDef::new(Products::Price).double().not_null())
        .col(ColumnDef::new(Products::Category).string())
        .to_owned();

    println!("CREATE TABLE:");
    println!("  {}", create.to_string(HyperQueryBuilder));
}
