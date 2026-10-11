// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tables and columns named after SQL reserved words (`order`, `user`, ...)
//! must work through every generated statement: CREATE, COPY, the mapped
//! INSERT...SELECT, DROP, and `derive(Table)`'s `CREATE_SQL`.

mod common;
use common::{TestConnection, test_hyper_params, test_result_path};

use hyperdb_api::{
    AsyncConnection, AsyncInserter, Catalog, ColumnMapping, CreateMode, HyperProcess, Inserter,
    SqlType, Table, TableDefinition,
};
use hyperdb_api_derive::Table;

fn reserved_def(table: &str) -> TableDefinition {
    TableDefinition::new(table)
        .add_required_column("order", SqlType::int())
        .add_nullable_column("user", SqlType::text())
}

fn count(test: &TestConnection, table: &str) -> i64 {
    test.connection
        .fetch_scalar::<i64, _>(&format!("SELECT COUNT(*) FROM \"{table}\""))
        .expect("count")
}

#[test]
fn sync_inserter_into_reserved_word_table() {
    let test = TestConnection::new().expect("connection");
    let def = reserved_def("order");
    Catalog::new(&test.connection)
        .create_table(&def)
        .expect("create");

    let mut ins = Inserter::new(&test.connection, &def).expect("inserter");
    ins.add_row(&[&1i32, &"a"]).expect("row");
    ins.add_row(&[&2i32, &"b"]).expect("row");
    assert_eq!(ins.execute().expect("COPY into \"order\""), 2);
    assert_eq!(count(&test, "order"), 2);

    Catalog::new(&test.connection)
        .drop_table("order")
        .expect("drop");
}

#[test]
fn drop_sql_quotes_reserved_word() {
    let def = reserved_def("user");
    assert_eq!(def.to_drop_sql(true), r#"DROP TABLE "user""#);
    assert_eq!(def.qualified_name(), r#""user""#);
}

#[test]
fn mapped_inserter_into_reserved_word_table() {
    let test = TestConnection::new().expect("connection");
    let target = reserved_def("table");
    Catalog::new(&test.connection)
        .create_table(&target)
        .expect("create");

    let stage = TableDefinition::new("_stage")
        .add_required_column("order", SqlType::int())
        .add_nullable_column("user", SqlType::text());
    let mappings = vec![ColumnMapping::new("order"), ColumnMapping::new("user")];
    let mut ins = Inserter::with_column_mappings(&test.connection, &stage, "table", &mappings)
        .expect("mapped inserter");
    ins.add_row(&[&7i32, &"x"]).expect("row");
    assert_eq!(ins.execute().expect("mapped insert"), 1);
    assert_eq!(count(&test, "table"), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn async_inserter_into_reserved_word_table() {
    let name = "reserved_word_async";
    let db_path = test_result_path(name, "hyper").expect("path");
    let params = test_hyper_params(name).expect("params");
    let hyper = HyperProcess::new(None, Some(&params)).expect("hyperd");
    let endpoint = hyper.require_endpoint().expect("endpoint").to_string();
    let conn = AsyncConnection::connect(
        &endpoint,
        db_path.to_str().expect("utf8"),
        CreateMode::CreateAndReplace,
    )
    .await
    .expect("connect");

    let def = reserved_def("select");
    conn.execute_command(&def.to_create_sql(true).expect("ddl"))
        .await
        .expect("create");
    let mut ins = AsyncInserter::new(&conn, &def).expect("inserter");
    ins.add_i32(1).unwrap();
    ins.add_str("a").unwrap();
    ins.end_row().await.expect("row");
    assert_eq!(ins.execute().await.expect("COPY into \"select\""), 1);
}

#[derive(Debug, Table)]
#[allow(dead_code, reason = "only the derived consts are used")]
#[hyperdb(table = "group")]
struct Reserved {
    order: i64,
    #[hyperdb(rename = "user")]
    who: Option<String>,
}

#[test]
fn derive_table_create_sql_quotes_reserved_words() {
    let test = TestConnection::new().expect("connection");
    assert_eq!(Reserved::NAME, "group");
    test.connection
        .execute_command(Reserved::CREATE_SQL)
        .expect("derive CREATE_SQL with reserved names must parse");
    assert_eq!(count(&test, "group"), 0);
}
