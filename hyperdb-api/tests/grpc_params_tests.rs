// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for JSON-encoded gRPC query parameters.
//!
//! hyperd only accepts JSON parameters as an array of typed entries
//! (`[{"type":"bigint","value":"42"}, ...]`), so these tests send the
//! output of [`QueryParameters::json_positional`] and
//! [`QueryParameters::json_named`] to a live `hyperd` and check the values
//! that come back.

use arrow::ipc::reader::StreamReader;
use arrow::util::display::array_value_to_string;
use hyperdb_api::{HyperProcess, ListenMode, Parameters, Result};
use hyperdb_api_core::client::grpc::{GrpcClientSync, GrpcConfig, ParameterStyle, QueryParameters};
use serde_json::json;

/// Spins up `hyperd` in gRPC-only mode and returns the process and its gRPC URL.
fn grpc_hyperd() -> Result<(HyperProcess, String)> {
    let mut params = Parameters::new();
    params.set("log_dir", "test_results");
    params.set_listen_mode(ListenMode::Grpc { port: 0 });
    let hyper = HyperProcess::new(None, Some(&params))?;
    let url = hyper
        .grpc_url()
        .expect("hyperd should expose a gRPC URL when listen_mode=Grpc");
    Ok((hyper, url))
}

/// Runs `sql` with `params` and returns the single result row, each column
/// rendered as a display string.
fn query_one_row(
    client: &mut GrpcClientSync,
    sql: &str,
    params: QueryParameters,
    style: ParameterStyle,
) -> Result<Vec<String>> {
    let result = client.execute_query_with_params(sql, params, style)?;
    let reader = StreamReader::try_new(std::io::Cursor::new(result.arrow_data()), None)
        .expect("result should be an Arrow IPC stream");
    let batches: Vec<_> = reader
        .collect::<std::result::Result<_, _>>()
        .expect("Arrow batches should decode");
    let batch = batches
        .iter()
        .find(|b| b.num_rows() > 0)
        .expect("query should return a row");
    assert_eq!(batch.num_rows(), 1, "expected exactly one row");
    Ok(batch
        .columns()
        .iter()
        .map(|col| array_value_to_string(col, 0).expect("value should format"))
        .collect())
}

#[test]
fn test_json_positional_question_mark_mixed_types() -> Result<()> {
    let (_hyper, url) = grpc_hyperd()?;
    let mut client = GrpcClientSync::connect(GrpcConfig::new(&url))?;

    let params =
        QueryParameters::json_positional(&[&json!(41), &json!("hi"), &json!(true), &json!(1.5)])
            .expect("parameters should serialize");
    let row = query_one_row(
        &mut client,
        "SELECT ? + 1 AS a, ? AS b, ? AS c, ? * 2 AS d",
        params,
        ParameterStyle::QuestionMark,
    )?;
    assert_eq!(row, ["42", "hi", "true", "3.0"]);
    Ok(())
}

#[test]
fn test_json_positional_dollar_numbered() -> Result<()> {
    let (_hyper, url) = grpc_hyperd()?;
    let mut client = GrpcClientSync::connect(GrpcConfig::new(&url))?;

    let params =
        QueryParameters::json_positional(&[&40i64, &2i64]).expect("parameters should serialize");
    let row = query_one_row(
        &mut client,
        "SELECT $1 + $2 AS s, $2 AS second",
        params,
        ParameterStyle::DollarNumbered,
    )?;
    assert_eq!(row, ["42", "2"]);
    Ok(())
}

#[test]
fn test_json_positional_null() -> Result<()> {
    let (_hyper, url) = grpc_hyperd()?;
    let mut client = GrpcClientSync::connect(GrpcConfig::new(&url))?;

    let params = QueryParameters::json_positional(&[&json!(null), &json!(null)])
        .expect("parameters should serialize");
    let row = query_one_row(
        &mut client,
        "SELECT ? IS NULL AS n, CAST(? AS BIGINT) IS NULL AS typed",
        params,
        ParameterStyle::QuestionMark,
    )?;
    assert_eq!(row, ["true", "true"]);
    Ok(())
}

#[test]
fn test_json_positional_u64_above_i64_max() -> Result<()> {
    let (_hyper, url) = grpc_hyperd()?;
    let mut client = GrpcClientSync::connect(GrpcConfig::new(&url))?;

    let params =
        QueryParameters::json_positional(&[&u64::MAX]).expect("parameters should serialize");
    let row = query_one_row(
        &mut client,
        "SELECT CAST(? AS TEXT) AS v",
        params,
        ParameterStyle::QuestionMark,
    )?;
    assert_eq!(row, ["18446744073709551615"]);
    Ok(())
}

#[test]
fn test_from_json_string_explicit_types() -> Result<()> {
    let (_hyper, url) = grpc_hyperd()?;
    let mut client = GrpcClientSync::connect(GrpcConfig::new(&url))?;

    let params = QueryParameters::from_json_string(
        r#"[{"type": "date", "value": "2026-01-31"},
            {"type": "numeric", "precision": 10, "scale": 2, "value": "12.50"}]"#,
    );
    let row = query_one_row(
        &mut client,
        "SELECT CAST($1 + 1 AS TEXT) AS d, CAST($2 AS TEXT) AS n",
        params,
        ParameterStyle::DollarNumbered,
    )?;
    assert_eq!(row, ["2026-02-01", "12.50"]);
    Ok(())
}

#[test]
fn test_json_named() -> Result<()> {
    let (_hyper, url) = grpc_hyperd()?;
    let mut client = GrpcClientSync::connect(GrpcConfig::new(&url))?;

    let params = QueryParameters::json_named()
        .add("id", &42i64)
        .expect("id should serialize")
        .add("user_name", &"Alice")
        .expect("user_name should serialize")
        .add_null("missing")
        // `name` is a keyword, so the SQL has to quote it
        .add("name", &"Bob")
        .expect("name should serialize")
        .build();
    let row = query_one_row(
        &mut client,
        r#"SELECT :id AS id, :user_name AS user_name, :missing IS NULL AS missing, :"name" AS n"#,
        params,
        ParameterStyle::Named,
    )?;
    assert_eq!(row, ["42", "Alice", "true", "Bob"]);
    Ok(())
}
