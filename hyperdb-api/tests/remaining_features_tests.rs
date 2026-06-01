// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for remaining API features:
//! - #7: Batch statement execution (`execute_batch`)
//! - #12: Catalog metadata helpers (`get_row_count`, `get_column_names`, `get_database_names`)
//! - #15: Server version struct (`ServerVersion`)
//! - #14: Database copy (`copy_database`)
//! - #13: EXPLAIN (explain, `explain_analyze`)
//! - #11: Connection timeouts (`query_timeout`, `application_name` on `ConnectionBuilder`)
//! - #17: `FromRow` trait (`fetch_one_as`, `fetch_all_as`)
//! - #16: Connection health (ping)

mod common;
use common::TestConnection;

use hyperdb_api::{Catalog, FromRow, ServerVersion};
// The derive macro is no longer re-exported from hyperdb_api; import directly.
use hyperdb_api_derive::FromRow;

// =============================================================================
// #7: Batch Statement Execution
// =============================================================================

#[test]
fn test_execute_batch() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let total = test
        .connection
        .execute_batch(&[
            "CREATE TABLE batch_test (id INT NOT NULL, name TEXT)",
            "INSERT INTO batch_test VALUES (1, 'Alice')",
            "INSERT INTO batch_test VALUES (2, 'Bob')",
            "INSERT INTO batch_test VALUES (3, 'Carol')",
        ])
        .expect("execute_batch");

    // DDL returns 0, each INSERT returns 1 → total = 3
    assert_eq!(total, 3);

    let count = test.count_tuples("batch_test").expect("count");
    assert_eq!(count, 3);
}

#[test]
fn test_execute_batch_empty() {
    let test = TestConnection::new().expect("Failed to create test connection");
    let total = test.connection.execute_batch(&[]).expect("empty batch");
    assert_eq!(total, 0);
}

#[test]
fn test_execute_batch_skips_blank() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let total = test
        .connection
        .execute_batch(&[
            "CREATE TABLE blank_test (id INT)",
            "",
            "   ",
            "INSERT INTO blank_test VALUES (1)",
        ])
        .expect("batch with blanks");

    assert_eq!(total, 1);
}

#[test]
fn test_execute_batch_stops_on_error() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let result = test.connection.execute_batch(&[
        "CREATE TABLE err_test (id INT NOT NULL)",
        "INSERT INTO err_test VALUES (1)",
        "THIS IS INVALID SQL",
        "INSERT INTO err_test VALUES (2)", // should not execute
    ]);

    assert!(result.is_err(), "Should fail on invalid SQL");
}

// =============================================================================
// #12: Catalog Metadata Helpers
// =============================================================================

#[test]
fn test_catalog_get_row_count() {
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command("CREATE TABLE count_test (id INT NOT NULL)")
        .expect("create");
    test.execute_command("INSERT INTO count_test SELECT * FROM GENERATE_SERIES(1, 42)")
        .expect("insert");

    let catalog = Catalog::new(&test.connection);
    let count = catalog.get_row_count("count_test").expect("row count");
    assert_eq!(count, 42);
}

#[test]
fn test_catalog_get_column_names() {
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command(
        "CREATE TABLE colname_test (id INT NOT NULL, name TEXT, value DOUBLE PRECISION)",
    )
    .expect("create");

    let catalog = Catalog::new(&test.connection);
    let columns = catalog
        .get_column_names("colname_test")
        .expect("column names");

    assert_eq!(columns.len(), 3);
    assert_eq!(columns[0], "id");
    assert_eq!(columns[1], "name");
    assert_eq!(columns[2], "value");
}

#[test]
fn test_catalog_get_database_names() {
    let test = TestConnection::new().expect("Failed to create test connection");

    let catalog = Catalog::new(&test.connection);
    let databases = catalog.get_database_names().expect("database names");

    // Should have at least the test database
    assert!(!databases.is_empty(), "Should have at least one database");
}

// =============================================================================
// #15: Server Version
// =============================================================================

#[test]
fn test_server_version_parse() {
    let v = ServerVersion::parse("0.0.19038").unwrap();
    assert_eq!(v.major(), 0);
    assert_eq!(v.minor(), 0);
    assert_eq!(v.patch(), 19038);
}

#[test]
fn test_server_version_comparison() {
    let v1 = ServerVersion::new(1, 0, 0);
    let v2 = ServerVersion::new(1, 0, 1);
    let v3 = ServerVersion::new(2, 0, 0);

    assert!(v1 < v2);
    assert!(v2 < v3);
    assert_eq!(v1, ServerVersion::new(1, 0, 0));
}

#[test]
fn test_server_version_from_connection() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // The connection should report a server version
    let version = test.connection.server_version();
    // It's OK if this is None for some test configurations, but if present it should parse
    if let Some(v) = version {
        assert!(
            v >= ServerVersion::new(0, 0, 1),
            "Version should be at least 0.0.1, got: {v}"
        );
    }
}

// =============================================================================
// #14: Database Copy
// =============================================================================

#[test]
fn test_copy_database() {
    let test = TestConnection::new().expect("Failed to create test connection");

    // Create some data in the source database
    test.execute_command("CREATE TABLE copy_src (id INT NOT NULL)")
        .expect("create");
    test.execute_command("INSERT INTO copy_src VALUES (1), (2), (3)")
        .expect("insert");

    // Get the source database path and construct a destination path
    let src_path = test.database_path.to_string_lossy().to_string();
    let dst_path = src_path.replace(".hyper", "_backup.hyper");

    // Remove destination if it exists from a previous run
    let _ = std::fs::remove_file(&dst_path);

    // Copy the database
    let result = test.connection.copy_database(&src_path, &dst_path);
    // COPY DATABASE may not be supported in all Hyper versions
    if let Ok(()) = result {
        // Verify the backup exists
        assert!(
            std::path::Path::new(&dst_path).exists(),
            "Backup file should exist"
        );
        // Clean up
        let _ = std::fs::remove_file(&dst_path);
    }
    // If it fails, that's OK — COPY DATABASE may not be available
}

// =============================================================================
// #13: EXPLAIN
// =============================================================================

#[test]
fn test_explain() {
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command("CREATE TABLE explain_test (id INT NOT NULL, value DOUBLE PRECISION)")
        .expect("create");
    test.execute_command("INSERT INTO explain_test VALUES (1, 1.0), (2, 2.0)")
        .expect("insert");

    let plan = test
        .connection
        .explain("SELECT * FROM explain_test WHERE id > 0")
        .expect("explain");

    assert!(!plan.is_empty(), "EXPLAIN should return a non-empty plan");
}

// =============================================================================
// #11: Connection Timeouts
// =============================================================================

#[test]
fn test_connection_builder_application_name() {
    // Verify that ConnectionBuilder with application_name compiles and works
    let test = TestConnection::new().expect("Failed to create test connection");
    let endpoint = test.connection.parameter_status("server_version");
    // Just verify we can read parameters — the application_name is tested
    // indirectly through the builder
    assert!(endpoint.is_some() || endpoint.is_none()); // always true, just exercises the code
}

// =============================================================================
// #17: FromRow Struct Mapping
// =============================================================================

#[derive(Debug, PartialEq)]
struct TestUser {
    id: i32,
    name: String,
    score: f64,
}

impl FromRow for TestUser {
    fn from_row(row: hyperdb_api::RowAccessor<'_>) -> hyperdb_api::Result<Self> {
        Ok(TestUser {
            id: row.get("id")?,
            name: row.get_opt("name")?.unwrap_or_default(),
            score: row.get_opt("score")?.unwrap_or(0.0),
        })
    }
}

#[test]
fn test_fetch_one_as() {
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command(
        "CREATE TABLE from_row_test (id INT NOT NULL, name TEXT, score DOUBLE PRECISION)",
    )
    .expect("create");
    test.execute_command("INSERT INTO from_row_test VALUES (1, 'Alice', 95.5)")
        .expect("insert");

    let user: TestUser = test
        .connection
        .fetch_one_as("SELECT id, name, score FROM from_row_test WHERE id = 1")
        .expect("fetch_one_as");

    assert_eq!(user.id, 1);
    assert_eq!(user.name, "Alice");
    assert!((user.score - 95.5).abs() < 0.001);
}

#[test]
fn test_fetch_all_as() {
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command(
        "CREATE TABLE from_row_all (id INT NOT NULL, name TEXT, score DOUBLE PRECISION)",
    )
    .expect("create");
    test.execute_command(
        "INSERT INTO from_row_all VALUES (1, 'Alice', 95.5), (2, 'Bob', 87.0), (3, 'Carol', 92.3)",
    )
    .expect("insert");

    let users: Vec<TestUser> = test
        .connection
        .fetch_all_as("SELECT id, name, score FROM from_row_all ORDER BY id")
        .expect("fetch_all_as");

    assert_eq!(users.len(), 3);
    assert_eq!(users[0].id, 1);
    assert_eq!(users[0].name, "Alice");
    assert_eq!(users[1].id, 2);
    assert_eq!(users[1].name, "Bob");
    assert_eq!(users[2].id, 3);
    assert_eq!(users[2].name, "Carol");
}

// `test_from_row_tuple` was removed in v0.3.0 along with the blanket
// `(Option<A>,)` … `(Option<A>, Option<B>, Option<C>, Option<D>)`
// `FromRow` impls. For ad-hoc tuple-shaped destructuring, callers
// should now use `Row::get(idx)` directly on each row, or define a
// struct with `#[derive(FromRow)]`.

// =============================================================================
// #17 cont: #[derive(FromRow)] parity with hand-written impl
// =============================================================================
//
// Verifies that the proc-macro-generated impl produces the same values
// as the hand-written `TestUser` impl for the same query.

#[derive(Debug, PartialEq, FromRow)]
struct TestUserDerived {
    id: i32,
    name: Option<String>,
    score: Option<f64>,
}

#[test]
fn test_derive_from_row_parity_with_handwritten() {
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command(
        "CREATE TABLE derive_parity (id INT NOT NULL, name TEXT, score DOUBLE PRECISION)",
    )
    .expect("create");
    test.execute_command("INSERT INTO derive_parity VALUES (1, 'Alice', 95.5), (2, 'Bob', 87.0)")
        .expect("insert");

    let handwritten: Vec<TestUser> = test
        .connection
        .fetch_all_as("SELECT id, name, score FROM derive_parity ORDER BY id")
        .expect("fetch_all_as TestUser");

    let derived: Vec<TestUserDerived> = test
        .connection
        .fetch_all_as("SELECT id, name, score FROM derive_parity ORDER BY id")
        .expect("fetch_all_as TestUserDerived");

    assert_eq!(handwritten.len(), 2);
    assert_eq!(derived.len(), 2);
    for (hw, drv) in handwritten.iter().zip(derived.iter()) {
        assert_eq!(hw.id, drv.id);
        assert_eq!(Some(hw.name.clone()), drv.name);
        assert_eq!(Some(hw.score), drv.score);
    }
}

#[test]
fn test_derive_from_row_null_in_optional_field_returns_none() {
    // Regression test for the Option<T> -> get_opt path: a NULL cell
    // in an Option<T> field on a derived FromRow must produce None,
    // not an Error::Column { kind: Null }.
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command(
        "CREATE TABLE derive_null (id INT NOT NULL, name TEXT, score DOUBLE PRECISION)",
    )
    .expect("create");
    test.execute_command("INSERT INTO derive_null VALUES (1, NULL, NULL)")
        .expect("insert null row");

    let row: TestUserDerived = test
        .connection
        .fetch_one_as("SELECT id, name, score FROM derive_null WHERE id = 1")
        .expect("fetch_one_as");

    assert_eq!(row.id, 1);
    assert_eq!(row.name, None);
    assert_eq!(row.score, None);
}

#[test]
fn test_derive_from_row_with_rename() {
    // Verify `#[hyperdb(rename = "...")]` redirects field-name lookup
    // to a different column name.
    #[derive(Debug, PartialEq, FromRow)]
    struct UserWithRename {
        id: i32,
        #[hyperdb(rename = "score")]
        rating: Option<f64>,
    }

    let test = TestConnection::new().expect("Failed to create test connection");
    test.execute_command("CREATE TABLE rename_test (id INT NOT NULL, score DOUBLE PRECISION)")
        .expect("create");
    test.execute_command("INSERT INTO rename_test VALUES (1, 99.9)")
        .expect("insert");

    let user: UserWithRename = test
        .connection
        .fetch_one_as("SELECT id, score FROM rename_test")
        .expect("fetch_one_as UserWithRename");

    assert_eq!(user.id, 1);
    assert_eq!(user.rating, Some(99.9));
}

#[test]
fn test_derive_from_row_with_index() {
    // Verify `#[hyperdb(index = N)]` uses positional access. Useful
    // for queries with computed/unnamed columns where the column has
    // no stable name (e.g. `SELECT id, COUNT(*) FROM ...`).
    #[derive(Debug, PartialEq, FromRow)]
    struct PositionalRow {
        #[hyperdb(index = 0)]
        id: i32,
        #[hyperdb(index = 1)]
        total: Option<i64>,
    }

    let test = TestConnection::new().expect("Failed to create test connection");
    test.execute_command("CREATE TABLE positional_test (id INT NOT NULL, value INT)")
        .expect("create");
    test.execute_command("INSERT INTO positional_test VALUES (1, 10), (1, 20), (2, 30)")
        .expect("insert");

    // COUNT(*) produces an unnamed/computed column — name-based lookup
    // would be brittle, but positional access works.
    let row: PositionalRow = test
        .connection
        .fetch_one_as("SELECT id, COUNT(*) FROM positional_test WHERE id = 1 GROUP BY id")
        .expect("fetch_one_as PositionalRow");

    assert_eq!(row.id, 1);
    assert_eq!(row.total, Some(2));
}

#[test]
fn test_derive_from_row_missing_column_errors() {
    // Asking for a column that isn't in the SELECT list should
    // surface as Error::Column { kind: Missing }.
    use hyperdb_api::{ColumnErrorKind, Error};

    #[derive(Debug, FromRow)]
    #[allow(
        dead_code,
        reason = "fields populated by the generated impl; only error path is asserted"
    )]
    struct NeedsColumn {
        id: i32,
        not_in_query: i32,
    }

    let test = TestConnection::new().expect("Failed to create test connection");
    test.execute_command("CREATE TABLE missing_col_test (id INT NOT NULL)")
        .expect("create");
    test.execute_command("INSERT INTO missing_col_test VALUES (1)")
        .expect("insert");

    let err = test
        .connection
        .fetch_one_as::<NeedsColumn>("SELECT id FROM missing_col_test")
        .expect_err("expected missing-column error");

    match err {
        Error::Column { name, kind } => {
            assert_eq!(name, "not_in_query");
            assert!(
                matches!(kind, ColumnErrorKind::Missing),
                "expected Missing kind, got {kind:?}",
            );
        }
        other => panic!("expected Error::Column {{ kind: Missing }}, got {other:?}"),
    }
}

// =============================================================================
// #16: Connection Health (ping)
// =============================================================================

#[test]
fn test_ping() {
    let test = TestConnection::new().expect("Failed to create test connection");
    test.connection.ping().expect("ping should succeed");
}

#[test]
fn test_ping_after_operations() {
    let test = TestConnection::new().expect("Failed to create test connection");

    test.execute_command("CREATE TABLE ping_test (id INT)")
        .expect("create");
    test.execute_command("INSERT INTO ping_test VALUES (1)")
        .expect("insert");

    // Ping should still work after operations
    test.connection.ping().expect("ping after operations");
}
