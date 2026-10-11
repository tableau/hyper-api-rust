// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for the full [`AsyncConnection`] API surface.
//!
//! These mirror the sync-side tests in [`connection_tests.rs`] and are
//! the regression harness for async-parity work.

mod common;

use common::{test_hyper_params, test_result_path};
use futures::{StreamExt, TryStreamExt};
use hyperdb_api::{AsyncConnection, CreateMode, FromRow, HyperProcess, Result};

async fn fresh_async_conn(name: &str) -> Result<(HyperProcess, AsyncConnection)> {
    let db_path = test_result_path(name, "hyper")?;
    let params = test_hyper_params(name)?;
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper.require_endpoint()?.to_string();
    let conn = AsyncConnection::connect(
        &endpoint,
        db_path.to_str().expect("path"),
        CreateMode::CreateAndReplace,
    )
    .await?;
    Ok((hyper, conn))
}

#[tokio::test(flavor = "current_thread")]
async fn execute_query_streaming_chunks() {
    let (_hyper, conn) = fresh_async_conn("async_exec_query_chunks").await.unwrap();

    conn.execute_command("CREATE TABLE t (v INT NOT NULL)")
        .await
        .unwrap();
    for i in 1..=8 {
        conn.execute_command(&format!("INSERT INTO t VALUES ({i})"))
            .await
            .unwrap();
    }

    let mut rs = conn
        .execute_query("SELECT v FROM t ORDER BY v")
        .await
        .unwrap();
    let mut total = 0;
    while let Some(chunk) = rs.next_chunk().await.unwrap() {
        total += chunk.len();
    }
    assert_eq!(total, 8);
    drop(rs);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_family_roundtrip() {
    let (_hyper, conn) = fresh_async_conn("async_fetch_family").await.unwrap();

    conn.execute_command("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .await
        .unwrap();
    conn.execute_command("INSERT INTO t VALUES (1, 'alice'), (2, 'bob'), (3, NULL)")
        .await
        .unwrap();

    // fetch_one
    let row = conn
        .fetch_one("SELECT id, name FROM t WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(row.get::<i32>(0), Some(1));
    assert_eq!(row.get::<String>(1), Some("alice".to_string()));

    // fetch_optional (hit)
    let row = conn
        .fetch_optional("SELECT id FROM t WHERE id = 2")
        .await
        .unwrap();
    assert!(row.is_some());

    // fetch_optional (miss)
    let row = conn
        .fetch_optional("SELECT id FROM t WHERE id = 999")
        .await
        .unwrap();
    assert!(row.is_none());

    // fetch_all
    let rows = conn
        .fetch_all("SELECT id FROM t ORDER BY id")
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);

    // fetch_scalar
    let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count, 3);

    // fetch_optional_scalar (hit)
    let name: Option<String> = conn
        .fetch_optional_scalar("SELECT name FROM t WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(name, Some("alice".to_string()));

    // query_count
    let n = conn
        .query_count("SELECT COUNT(*) FROM t WHERE name IS NOT NULL")
        .await
        .unwrap();
    assert_eq!(n, 2);

    conn.close().await.unwrap();
}

#[derive(Debug, PartialEq)]
struct User {
    id: i32,
    name: Option<String>,
}

impl FromRow for User {
    fn from_row(row: hyperdb_api::RowAccessor<'_>) -> Result<Self> {
        Ok(User {
            id: row.get("id")?,
            name: row.get_opt("name")?,
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_as_struct_mapping() {
    let (_hyper, conn) = fresh_async_conn("async_fetch_as").await.unwrap();

    conn.execute_command("CREATE TABLE users (id INT NOT NULL, name TEXT)")
        .await
        .unwrap();
    conn.execute_command("INSERT INTO users VALUES (1, 'alice'), (2, 'bob')")
        .await
        .unwrap();

    let user: User = conn
        .fetch_one_as("SELECT id, name FROM users WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(
        user,
        User {
            id: 1,
            name: Some("alice".to_string())
        }
    );

    let users: Vec<User> = conn
        .fetch_all_as("SELECT id, name FROM users ORDER BY id")
        .await
        .unwrap();
    assert_eq!(users.len(), 2);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn query_and_command_params() {
    let (_hyper, conn) = fresh_async_conn("async_params").await.unwrap();

    conn.execute_command("CREATE TABLE orders (id INT NOT NULL, total DOUBLE PRECISION)")
        .await
        .unwrap();

    // command_params (INSERT)
    let n = conn
        .command_params("INSERT INTO orders VALUES ($1, $2)", &[&1i32, &99.5_f64])
        .await
        .unwrap();
    assert_eq!(n, 1);

    // command_params (INSERT another)
    conn.command_params("INSERT INTO orders VALUES ($1, $2)", &[&2i32, &200.0_f64])
        .await
        .unwrap();

    // query_params (SELECT with WHERE)
    let rs = conn
        .query_params(
            "SELECT id FROM orders WHERE total > $1 ORDER BY id",
            &[&100.0_f64],
        )
        .await
        .unwrap();
    let rows = rs.collect_rows().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<i32>(0), Some(2));

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn has_table_and_schema() {
    let (_hyper, conn) = fresh_async_conn("async_catalog").await.unwrap();

    assert!(!conn.has_table("nope").await.unwrap());
    conn.execute_command("CREATE TABLE kept (id INT)")
        .await
        .unwrap();
    assert!(conn.has_table("kept").await.unwrap());

    assert!(conn.has_schema("public").await.unwrap());
    assert!(!conn.has_schema("nonexistent_schema").await.unwrap());

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn ping_and_version() {
    let (_hyper, conn) = fresh_async_conn("async_ping").await.unwrap();

    conn.ping().await.unwrap();
    assert!(conn.is_alive());
    // server_version is best-effort — hyperd sets it but older builds
    // may omit; just make sure the getter works.
    let _ = conn.server_version().await;

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn execute_batch_runs_all_statements() {
    let (_hyper, conn) = fresh_async_conn("async_batch").await.unwrap();

    let total = conn
        .execute_batch(&[
            "CREATE TABLE b (id INT NOT NULL)",
            "INSERT INTO b VALUES (1)",
            "INSERT INTO b VALUES (2)",
        ])
        .await
        .unwrap();
    assert!(total >= 2);

    let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM b").await.unwrap();
    assert_eq!(count, 2);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_with_commit() {
    let (_hyper, mut conn) = fresh_async_conn("async_tx_commit").await.unwrap();

    conn.execute_command("CREATE TABLE t (v INT NOT NULL)")
        .await
        .unwrap();
    {
        let txn = conn.transaction().await.unwrap();
        txn.execute_command("INSERT INTO t VALUES (1)")
            .await
            .unwrap();
        txn.execute_command("INSERT INTO t VALUES (2)")
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
    let count: i64 = conn.fetch_scalar("SELECT COUNT(*) FROM t").await.unwrap();
    assert_eq!(count, 2);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn stream_as_happy_path() {
    let (_hyper, conn) = fresh_async_conn("async_stream_as_happy").await.unwrap();

    conn.execute_command("CREATE TABLE users (id INT NOT NULL, name TEXT)")
        .await
        .unwrap();
    conn.execute_command("INSERT INTO users VALUES (1, 'alice'), (2, 'bob'), (3, NULL)")
        .await
        .unwrap();

    let users = {
        let stream = conn.stream_as::<User>("SELECT id, name FROM users ORDER BY id");
        tokio::pin!(stream);
        stream.try_collect::<Vec<User>>().await.unwrap()
    };

    assert_eq!(users.len(), 3);
    assert_eq!(
        users[0],
        User {
            id: 1,
            name: Some("alice".to_string())
        }
    );
    assert_eq!(
        users[1],
        User {
            id: 2,
            name: Some("bob".to_string())
        }
    );
    assert_eq!(users[2], User { id: 3, name: None });

    // Verify it matches fetch_all_as
    let fetch_all: Vec<User> = conn
        .fetch_all_as("SELECT id, name FROM users ORDER BY id")
        .await
        .unwrap();
    assert_eq!(users, fetch_all);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn stream_as_multi_chunk() {
    let (_hyper, conn) = fresh_async_conn("async_stream_as_multi_chunk")
        .await
        .unwrap();

    conn.execute_command("CREATE TABLE big (id INT NOT NULL, name TEXT)")
        .await
        .unwrap();
    // The TCP client accumulates up to DEFAULT_BINARY_CHUNK_SIZE (65_536) rows
    // per chunk, so insert > 2× that to force at least two non-empty chunks and
    // genuinely exercise the cross-chunk re-entry path (index map built once,
    // reused on the second chunk).
    const ROWS: i32 = 140_000;
    conn.execute_command(&format!(
        "INSERT INTO big SELECT id, 'user_' || id::TEXT FROM GENERATE_SERIES(1, {ROWS}) AS id",
    ))
    .await
    .unwrap();

    let users = {
        let stream = conn.stream_as::<User>("SELECT id, name FROM big ORDER BY id");
        tokio::pin!(stream);
        stream.try_collect::<Vec<User>>().await.unwrap()
    };

    let last = usize::try_from(ROWS).expect("row count fits usize") - 1;
    assert_eq!(users.len(), ROWS as usize);
    // Verify first and last
    assert_eq!(
        users[0],
        User {
            id: 1,
            name: Some("user_1".to_string())
        }
    );
    assert_eq!(
        users[last],
        User {
            id: ROWS,
            name: Some(format!("user_{ROWS}"))
        }
    );

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn stream_as_submit_error() {
    let (_hyper, conn) = fresh_async_conn("async_stream_as_submit_error")
        .await
        .unwrap();

    // Query a nonexistent table
    {
        let stream = conn.stream_as::<User>("SELECT id, name FROM nonexistent_table");
        tokio::pin!(stream);

        // The first item should be an Err (submit error surfaces lazily)
        let first = stream.next().await;
        assert!(first.is_some());
        assert!(first.unwrap().is_err());
    }

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn stream_as_empty() {
    let (_hyper, conn) = fresh_async_conn("async_stream_as_empty").await.unwrap();

    conn.execute_command("CREATE TABLE empty_users (id INT NOT NULL, name TEXT)")
        .await
        .unwrap();

    let users = {
        let stream = conn.stream_as::<User>("SELECT id, name FROM empty_users WHERE 1=0");
        tokio::pin!(stream);
        stream.try_collect::<Vec<User>>().await.unwrap()
    };

    assert_eq!(users.len(), 0);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn stream_as_lenient_extra_column() {
    let (_hyper, conn) = fresh_async_conn("async_stream_as_lenient").await.unwrap();

    conn.execute_command("CREATE TABLE users_extra (id INT NOT NULL, name TEXT, extra TEXT)")
        .await
        .unwrap();
    conn.execute_command("INSERT INTO users_extra VALUES (1, 'alice', 'data')")
        .await
        .unwrap();

    // SELECT * includes the extra column, but User only maps id and name
    let users = {
        let stream = conn.stream_as::<User>("SELECT * FROM users_extra");
        tokio::pin!(stream);
        stream.try_collect::<Vec<User>>().await.unwrap()
    };

    assert_eq!(users.len(), 1);
    assert_eq!(
        users[0],
        User {
            id: 1,
            name: Some("alice".to_string())
        }
    );

    conn.close().await.unwrap();
}

// =============================================================================
// #137: Parameterized FromRow mapping (async)
// fetch_one_as_params / fetch_all_as_params / stream_as_params
// =============================================================================

async fn seed_param_users(conn: &AsyncConnection) {
    conn.execute_command(
        "CREATE TABLE param_users (id INT NOT NULL, org_id INT NOT NULL, name TEXT)",
    )
    .await
    .unwrap();
    conn.execute_command(
        "INSERT INTO param_users VALUES (1, 10, 'alice'), (2, 10, 'bob'), (3, 20, 'carol')",
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_one_as_params_async() {
    let (_hyper, conn) = fresh_async_conn("async_fetch_one_as_params").await.unwrap();
    seed_param_users(&conn).await;

    let user: User = conn
        .fetch_one_as_params("SELECT id, name FROM param_users WHERE id = $1", &[&2i32])
        .await
        .unwrap();
    assert_eq!(
        user,
        User {
            id: 2,
            name: Some("bob".to_string())
        }
    );

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_all_as_params_async() {
    let (_hyper, conn) = fresh_async_conn("async_fetch_all_as_params").await.unwrap();
    seed_param_users(&conn).await;

    let users: Vec<User> = conn
        .fetch_all_as_params(
            "SELECT id, name FROM param_users WHERE org_id = $1 ORDER BY id",
            &[&10i32],
        )
        .await
        .unwrap();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0].id, 1);
    assert_eq!(users[1].id, 2);

    conn.close().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn stream_as_params_async() {
    let (_hyper, conn) = fresh_async_conn("async_stream_as_params").await.unwrap();
    seed_param_users(&conn).await;

    let users = {
        let stream = conn.stream_as_params::<User>(
            "SELECT id, name FROM param_users WHERE org_id = $1 ORDER BY id",
            &[&10i32],
        );
        tokio::pin!(stream);
        stream.try_collect::<Vec<User>>().await.unwrap()
    };

    assert_eq!(users.len(), 2);
    assert_eq!(users[0].id, 1);
    assert_eq!(users[1].id, 2);

    conn.close().await.unwrap();
}

/// `copy_in_hyperbinary` streams a pre-encoded `HyperBinary` buffer into a
/// table and reports the inserted row count; an empty buffer is a no-op.
#[tokio::test(flavor = "current_thread")]
async fn copy_in_hyperbinary_inserts_encoded_rows() {
    use hyperdb_api::{InsertChunk, SqlType, TableDefinition};

    let (_hyper, conn) = fresh_async_conn("async_copy_in_hyperbinary").await.unwrap();
    let table_def = TableDefinition::from("copied")
        .add_required_column("id", SqlType::int())
        .add_required_column("name", SqlType::text());
    conn.execute_command("CREATE TABLE copied (id INT NOT NULL, name TEXT NOT NULL)")
        .await
        .unwrap();

    let mut chunk = InsertChunk::from_table_definition(&table_def);
    for (id, name) in [(1, "a"), (2, "b"), (3, "c")] {
        chunk.add_i32(id).unwrap();
        chunk.add_str(name).unwrap();
        chunk.end_row().unwrap();
    }
    let buffer = chunk.take().expect("rows were buffered");

    assert_eq!(conn.copy_in_hyperbinary(&table_def, &[]).await.unwrap(), 0);
    let inserted = conn.copy_in_hyperbinary(&table_def, &buffer).await.unwrap();
    assert_eq!(inserted, 3);

    let count: i64 = conn
        .fetch_scalar("SELECT COUNT(*) FROM copied")
        .await
        .unwrap();
    assert_eq!(count, 3);
}

/// Only the first `InsertChunk::take()` carries the `HyperBinary` header, so
/// a later buffer is not a complete COPY stream. It is refused up front with
/// `InvalidOperation` rather than failing mid-COPY on the server.
#[tokio::test(flavor = "current_thread")]
async fn copy_in_hyperbinary_rejects_headerless_buffer() {
    use hyperdb_api::{Error, InsertChunk, SqlType, TableDefinition};

    let (_hyper, conn) = fresh_async_conn("async_copy_in_headerless").await.unwrap();
    let table_def = TableDefinition::from("headerless").add_required_column("id", SqlType::int());
    conn.execute_command("CREATE TABLE headerless (id INT NOT NULL)")
        .await
        .unwrap();

    let mut chunk = InsertChunk::from_table_definition(&table_def);
    chunk.add_i32(1).unwrap();
    chunk.end_row().unwrap();
    let first = chunk.take().expect("first buffer");
    chunk.add_i32(2).unwrap();
    chunk.end_row().unwrap();
    let second = chunk.take().expect("second buffer");

    let err = conn
        .copy_in_hyperbinary(&table_def, &second)
        .await
        .expect_err("a headerless buffer is not a COPY stream");
    assert!(
        matches!(err, Error::InvalidOperation(_)),
        "expected InvalidOperation, got {err:?}"
    );

    // The connection is untouched, and the first buffer still works.
    assert_eq!(
        conn.copy_in_hyperbinary(&table_def, &first).await.unwrap(),
        1
    );
}

/// A server-side COPY failure surfaces as an error and leaves the connection
/// usable for the next statement.
#[tokio::test(flavor = "current_thread")]
async fn copy_in_hyperbinary_server_error_leaves_connection_usable() {
    use hyperdb_api::{SqlType, TableDefinition};
    use hyperdb_api_core::protocol::copy::HYPER_BINARY_HEADER;

    let (_hyper, conn) = fresh_async_conn("async_copy_in_server_error")
        .await
        .unwrap();
    let table_def = TableDefinition::from("broken").add_required_column("id", SqlType::int());
    conn.execute_command("CREATE TABLE broken (id INT NOT NULL)")
        .await
        .unwrap();

    // A valid header followed by 3 bytes: a truncated `INT` row.
    let mut data = HYPER_BINARY_HEADER.to_vec();
    data.extend_from_slice(&[0x01; 3]);
    conn.copy_in_hyperbinary(&table_def, &data)
        .await
        .expect_err("a truncated row must be rejected");

    let one: i64 = conn.fetch_scalar("SELECT 1").await.unwrap();
    assert_eq!(one, 1, "connection must stay usable after a failed COPY");
}

/// COPY needs the TCP wire protocol; a gRPC connection reports that instead
/// of attempting it.
#[tokio::test(flavor = "current_thread")]
async fn copy_in_hyperbinary_on_grpc_is_feature_not_supported() {
    use hyperdb_api::{Error, ListenMode, Parameters, SqlType, TableDefinition};
    use hyperdb_api_core::protocol::copy::HYPER_BINARY_HEADER;

    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    params.set_listen_mode(ListenMode::Grpc { port: 0 });
    let hyper = HyperProcess::new(None, Some(&params)).unwrap();
    let url = hyper.grpc_url().expect("gRPC URL");
    let conn = AsyncConnection::connect(&url, "", CreateMode::DoNotCreate)
        .await
        .unwrap();

    let table_def = TableDefinition::from("t").add_required_column("id", SqlType::int());
    let err = conn
        .copy_in_hyperbinary(&table_def, HYPER_BINARY_HEADER)
        .await
        .expect_err("COPY over gRPC is not supported");
    assert!(
        matches!(err, Error::FeatureNotSupported(_)),
        "expected FeatureNotSupported, got {err:?}"
    );
}

/// The string-endpoint constructors reach a Unix domain socket like
/// `connect` and the builder do, rather than parsing the path as `host:port`.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn string_constructors_connect_over_a_unix_socket() {
    use hyperdb_api::TransportMode;

    let mut params = test_hyper_params("async_string_constructors_ipc").unwrap();
    params.set_transport_mode(TransportMode::Ipc);
    let hyper = HyperProcess::new(None, Some(&params)).unwrap();
    let endpoint = hyper.connection_endpoint_string().expect("IPC endpoint");
    assert!(endpoint.starts_with('/'), "not a socket path: {endpoint}");

    let conn = AsyncConnection::without_database(&endpoint).await.unwrap();
    let one: i64 = conn.fetch_scalar("SELECT 1").await.unwrap();
    assert_eq!(one, 1);

    let db = test_result_path("async_string_constructors_ipc", "hyper").unwrap();
    let conn = AsyncConnection::connect_with_auth(
        &endpoint,
        db.to_str().expect("path"),
        CreateMode::CreateAndReplace,
        "tableau_internal_user",
        "unused",
    )
    .await
    .unwrap();
    assert_eq!(conn.database(), db.to_str());
}
