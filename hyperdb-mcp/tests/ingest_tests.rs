// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for inline data ingest (JSON and CSV): basic round-trips, append
//! mode, schema overrides, and error handling for empty input.

mod common;
use common::TestEngine;
use hyperdb_mcp::ingest::{
    detect_file_format, extract_json_path, ingest_csv, ingest_csv_file, ingest_json,
    ingest_json_file, InferredFileFormat, IngestOptions,
};
use std::io::Write;
use tempfile::TempPath;

/// Ingest a JSON array of two objects, then query back to verify rows were
/// inserted with correct column values and ordering.
#[test]
fn ingest_json_basic() {
    let te = TestEngine::new_ephemeral();
    let data = r#"[{"id": 1, "name": "Alice"}, {"id": 2, "name": "Bob"}]"#;
    let opts = IngestOptions {
        table: "users".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_json(&te.engine, data, &opts).unwrap();
    assert_eq!(result.rows, 2);

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM users ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "Alice");
}

/// Verify that append mode adds rows to an existing table without dropping it.
/// First ingest creates the table with 1 row, second ingest appends 1 more.
#[test]
fn ingest_json_append_mode() {
    let te = TestEngine::new_ephemeral();
    let data1 = r#"[{"id": 1}]"#;
    let data2 = r#"[{"id": 2}]"#;
    let opts_replace = IngestOptions {
        table: "t".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let opts_append = IngestOptions {
        table: "t".into(),
        mode: "append".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, data1, &opts_replace).unwrap();
    ingest_json(&te.engine, data2, &opts_append).unwrap();

    let count: i64 = te
        .engine
        .connection()
        .execute_scalar_query("SELECT COUNT(*) FROM t")
        .unwrap()
        .unwrap();
    assert_eq!(count, 2);
}

/// Ingest inline CSV text with a header row and verify the COPY FROM path
/// correctly loads both data rows.
#[test]
fn ingest_csv_basic() {
    let te = TestEngine::new_ephemeral();
    let csv_text = "id,name,score\n1,Alice,95.5\n2,Bob,88.0\n";
    let opts = IngestOptions {
        table: "scores".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_csv(&te.engine, csv_text, &opts).unwrap();
    assert_eq!(result.rows, 2);

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM scores ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 2);
}

/// Verify that a schema override forces the column type, bypassing inference.
/// Here a string value "123.45" is inserted into a DOUBLE PRECISION column
/// declared by the override rather than being inferred as TEXT.
#[test]
fn ingest_json_with_schema_override() {
    let te = TestEngine::new_ephemeral();
    let data = r#"[{"amount": "123.45"}]"#;
    let mut schema = serde_json::Map::new();
    schema.insert(
        "amount".into(),
        serde_json::Value::String("DOUBLE PRECISION".into()),
    );
    let opts = IngestOptions {
        table: "orders".into(),
        mode: "replace".into(),
        schema_override: Some(schema),
        merge_key: None,
        target_db: None,
    };
    let result = ingest_json(&te.engine, data, &opts).unwrap();
    assert_eq!(result.rows, 1);
}

/// Verify that ingesting an empty JSON array returns an error rather than
/// silently creating a table with no columns.
#[test]
fn ingest_json_empty_returns_error() {
    let te = TestEngine::new_ephemeral();
    let data = "[]";
    let opts = IngestOptions {
        table: "empty".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_json(&te.engine, data, &opts);
    assert!(result.is_err());
}

/// Helper: write `content` to a temp file with the given extension,
/// keep the file alive for the caller's lifetime, and return its path
/// as a String.
///
/// Returns a [`TempPath`] (not a [`NamedTempFile`]) so the file handle
/// is closed after writing. On Windows, hyperd's `COPY FROM` runs in a
/// separate process and can't open a file that the test still holds
/// open for writing; the Unix sharing rules are more permissive so
/// keeping the handle would only break the file-based CSV test, but
/// we standardize on the closed-handle form to keep all tests
/// cross-platform.
fn tmp_with_ext(ext: &str, content: &[u8]) -> (String, TempPath) {
    let mut file = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .expect("create temp file");
    file.write_all(content).expect("write temp");
    file.flush().ok();
    let temp_path = file.into_temp_path();
    let path = temp_path.to_str().expect("utf-8 path").to_string();
    (path, temp_path)
}

/// `ingest_json_file` loads a `.json` file containing a top-level array
/// of objects — the format produced by typical REST API snapshots.
#[test]
fn ingest_json_file_loads_json_array() {
    let te = TestEngine::new_ephemeral();
    let (path, _keep) = tmp_with_ext(
        "json",
        b"[{\"id\":1,\"name\":\"Alice\"},{\"id\":2,\"name\":\"Bob\"},{\"id\":3,\"name\":\"Carol\"}]",
    );
    let opts = IngestOptions {
        table: "people".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_json_file(&te.engine, &path, &opts).unwrap();
    assert_eq!(result.rows, 3);
    assert_eq!(result.stats.file_format.as_deref(), Some("json"));
    assert_eq!(result.stats.operation, "load_file");

    let rows = te
        .engine
        .execute_query_to_json("SELECT id, name FROM people ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1]["name"], "Bob");
}

/// `ingest_json_file` auto-detects newline-delimited JSON (JSONL /
/// NDJSON) — the format hyperd itself uses for its logs. Blank lines
/// are tolerated so real-world log files load without preprocessing.
#[test]
fn ingest_json_file_loads_jsonl() {
    let te = TestEngine::new_ephemeral();
    let jsonl = b"{\"k\":\"start\",\"n\":1}\n\
                  \n\
                  {\"k\":\"progress\",\"n\":2}\n\
                  {\"k\":\"done\",\"n\":3}\n";
    let (path, _keep) = tmp_with_ext("jsonl", jsonl);
    let opts = IngestOptions {
        table: "events".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_json_file(&te.engine, &path, &opts).unwrap();
    assert_eq!(result.rows, 3, "blank lines are skipped, data rows count 3");
    assert_eq!(result.stats.file_format.as_deref(), Some("jsonl"));

    let rows = te
        .engine
        .execute_query_to_json("SELECT k, n FROM events ORDER BY n")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["k"], "start");
    assert_eq!(rows[2]["k"], "done");
}

/// A malformed JSONL line surfaces a `SchemaMismatch` error that names
/// the offending line number, not a cryptic byte offset.
#[test]
fn ingest_json_file_reports_bad_jsonl_line() {
    let te = TestEngine::new_ephemeral();
    let bad = b"{\"id\":1}\n\
                {\"id\":2}\n\
                not-json\n";
    let (path, _keep) = tmp_with_ext("jsonl", bad);
    let opts = IngestOptions {
        table: "t".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let Err(err) = ingest_json_file(&te.engine, &path, &opts) else {
        panic!("expected malformed JSONL to error")
    };
    assert_eq!(err.code, hyperdb_mcp::error::ErrorCode::SchemaMismatch);
    assert!(
        err.message.contains("line 3"),
        "error should name the bad line: {}",
        err.message
    );
}

/// CSV ingest must load unquoted empty cells as SQL NULL, matching
/// `PostgreSQL`'s CSV default (and `inspect_file`'s `null_count`
/// accounting). Regression guard for the ",," = NULL contract that
/// users rely on when filtering with `WHERE col IS NULL`.
#[test]
fn ingest_csv_empty_cells_become_null() {
    let te = TestEngine::new_ephemeral();
    // Rows 1/3 have age set, row 2 leaves age empty. The empty cell
    // should land as SQL NULL, not an empty-string zero or a parse
    // error on the numeric column.
    let csv_text = "id,name,age\n1,Alice,30\n2,Bob,\n3,Carol,42\n";
    let opts = IngestOptions {
        table: "u".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_csv(&te.engine, csv_text, &opts).unwrap();
    assert_eq!(result.rows, 3);

    let nulls: i64 = te
        .engine
        .connection()
        .execute_scalar_query("SELECT COUNT(*) FROM u WHERE age IS NULL")
        .unwrap()
        .unwrap();
    assert_eq!(nulls, 1, "exactly one row should have a NULL age");

    let non_nulls: i64 = te
        .engine
        .connection()
        .execute_scalar_query("SELECT COUNT(*) FROM u WHERE age IS NOT NULL")
        .unwrap()
        .unwrap();
    assert_eq!(non_nulls, 2);
}

// --- Format detection ------------------------------------------------------

/// Known binary extensions win immediately without reading the file, so
/// the classifier is effectively free for Parquet / Arrow IPC paths.
#[test]
fn detect_file_format_matches_binary_extensions() {
    assert_eq!(
        detect_file_format(std::path::Path::new("/tmp/x.parquet")),
        InferredFileFormat::Parquet
    );
    assert_eq!(
        detect_file_format(std::path::Path::new("/tmp/x.pq")),
        InferredFileFormat::Parquet
    );
    assert_eq!(
        detect_file_format(std::path::Path::new("/tmp/x.arrow")),
        InferredFileFormat::ArrowIpc
    );
    assert_eq!(
        detect_file_format(std::path::Path::new("/tmp/x.ipc")),
        InferredFileFormat::ArrowIpc
    );
    assert_eq!(
        detect_file_format(std::path::Path::new("/tmp/x.feather")),
        InferredFileFormat::ArrowIpc
    );
}

/// JSON extensions (including `.ndjson`) map to Json without reading
/// the file. Covers the common case where a producer names the file
/// correctly.
#[test]
fn detect_file_format_matches_json_extensions() {
    for ext in ["json", "jsonl", "ndjson"] {
        let (path, _keep) = tmp_with_ext(ext, b"[{\"a\":1}]");
        assert_eq!(
            detect_file_format(std::path::Path::new(&path)),
            InferredFileFormat::Json,
            "`.{ext}` should map to Json without content sniff"
        );
    }
}

/// Content sniff fallback: a `.log` file whose first non-whitespace
/// byte is `{` dispatches to Json. This is the hyperd-log case — no
/// rename required.
#[test]
fn detect_file_format_sniffs_jsonl_from_unknown_extension() {
    let (path, _keep) = tmp_with_ext(
        "log",
        b"{\"ts\":\"2026-01-01T00:00:00\",\"k\":\"start\"}\n\
          {\"ts\":\"2026-01-01T00:00:01\",\"k\":\"done\"}\n",
    );
    assert_eq!(
        detect_file_format(std::path::Path::new(&path)),
        InferredFileFormat::Json
    );
}

/// A `.log` file whose first byte is `[` also sniffs as Json (JSON
/// array shape). Exercises the other JSON branch of the sniffer.
#[test]
fn detect_file_format_sniffs_json_array_from_unknown_extension() {
    let (path, _keep) = tmp_with_ext("log", b"  \n  [{\"k\":\"v\"}]\n");
    assert_eq!(
        detect_file_format(std::path::Path::new(&path)),
        InferredFileFormat::Json,
        "leading whitespace before `[` should still sniff as JSON"
    );
}

/// A file whose first non-whitespace byte is neither `[` nor `{` falls
/// back to CSV. Covers the common `.csv` / `.tsv` / unlabeled tabular
/// cases.
#[test]
fn detect_file_format_falls_back_to_csv() {
    let (path, _keep) = tmp_with_ext("txt", b"id,name,age\n1,Alice,30\n");
    assert_eq!(
        detect_file_format(std::path::Path::new(&path)),
        InferredFileFormat::Csv
    );
}

/// A nonexistent path is treated as CSV (the subsequent CSV ingest
/// will produce a cleaner "file not found" error than a classifier
/// failure would).
#[test]
fn detect_file_format_defaults_to_csv_when_unreadable() {
    assert_eq!(
        detect_file_format(std::path::Path::new("/tmp/definitely-not-a-real-file.xyz")),
        InferredFileFormat::Csv
    );
}

/// End-to-end: a JSONL file with a `.log` extension (hyperd's native
/// shape) loads cleanly through `ingest_json_file` via content sniff.
/// Regression guard for the "rename the log to `.jsonl` first" UX wart
/// this whole fix set was designed to remove.
#[test]
fn ingest_json_file_handles_log_extension_via_content_sniff() {
    let te = TestEngine::new_ephemeral();
    // Construct the payload through `detect_file_format` so the test
    // mirrors the production dispatch path rather than hard-coding a
    // specific ingest function.
    let (path, _keep) = tmp_with_ext(
        "log",
        b"{\"id\":1,\"msg\":\"hello\"}\n\
          {\"id\":2,\"msg\":\"world\"}\n",
    );
    let fmt = detect_file_format(std::path::Path::new(&path));
    assert_eq!(fmt, InferredFileFormat::Json);

    let opts = IngestOptions {
        table: "events".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_json_file(&te.engine, &path, &opts).unwrap();
    assert_eq!(result.rows, 2);
    assert_eq!(result.stats.file_format.as_deref(), Some("jsonl"));
}

/// Same contract for file-based CSV ingest: empty text cells become
/// NULL, so a downstream `WHERE col <> ''` does not need to carry a
/// defensive `OR col IS NULL` clause.
#[test]
fn ingest_csv_file_empty_cells_become_null() {
    let te = TestEngine::new_ephemeral();
    let (path, _keep) = tmp_with_ext("csv", b"code,label\nAAA,first\n,middle\nBBB,\n");
    let opts = IngestOptions {
        table: "lookup".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_csv_file(&te.engine, &path, &opts).unwrap();
    assert_eq!(result.rows, 3);

    let code_nulls: i64 = te
        .engine
        .connection()
        .execute_scalar_query("SELECT COUNT(*) FROM lookup WHERE code IS NULL")
        .unwrap()
        .unwrap();
    let label_nulls: i64 = te
        .engine
        .connection()
        .execute_scalar_query("SELECT COUNT(*) FROM lookup WHERE label IS NULL")
        .unwrap()
        .unwrap();
    assert_eq!(code_nulls, 1, "row 2's empty code should be NULL");
    assert_eq!(label_nulls, 1, "row 3's empty label should be NULL");
}

// ---------- extract_json_path tests ----------

/// Navigate a single key to reach an array.
#[test]
fn extract_json_path_simple_object() {
    let json = r#"{"results": [{"a": 1}, {"a": 2}]}"#;
    let out = extract_json_path(json, "results").unwrap();
    let arr: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(arr.len(), 2);
}

/// Navigate into an array with a numeric index.
#[test]
fn extract_json_path_array_index() {
    let json = r#"{"content": [{"type": "text"}, {"type": "image"}]}"#;
    let out = extract_json_path(json, "content.0").unwrap();
    let obj: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(obj["type"], "text");
}

/// The key scenario: Splunk MCP wrapper with stringified JSON in the
/// `text` field. `content.0.text` is a string that must be parsed as
/// JSON before navigating further into `query_result.results`.
#[test]
fn extract_json_path_stringified_json() {
    let inner = serde_json::json!({
        "status": "success",
        "query_result": {
            "results": [
                {"_time": "2026-01-01T00:00:00Z", "_raw": "log line 1"},
                {"_time": "2026-01-01T00:01:00Z", "_raw": "log line 2"},
            ]
        }
    });
    let wrapper = serde_json::json!({
        "content": [{
            "type": "text",
            "text": inner.to_string()
        }]
    });

    let out =
        extract_json_path(&wrapper.to_string(), "content.0.text.query_result.results").unwrap();
    let arr: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["_raw"], "log line 1");
}

/// Terminal string auto-parse: the value at the path is itself a
/// stringified JSON array.
#[test]
fn extract_json_path_terminal_string_parsed() {
    let json = r#"{"data": "[{\"a\":1},{\"a\":2}]"}"#;
    let out = extract_json_path(json, "data").unwrap();
    let arr: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["a"], 1);
}

/// Missing key returns a descriptive error.
#[test]
fn extract_json_path_missing_key() {
    let json = r#"{"other": 1}"#;
    let err = extract_json_path(json, "missing").unwrap_err();
    assert!(
        err.message.contains("key not found"),
        "error should mention missing key: {}",
        err.message
    );
}

/// Array index out of bounds returns a descriptive error.
#[test]
fn extract_json_path_index_out_of_bounds() {
    let json = r#"{"arr": [1, 2]}"#;
    let err = extract_json_path(json, "arr.5").unwrap_err();
    assert!(
        err.message.contains("out of bounds"),
        "error should mention out of bounds: {}",
        err.message
    );
}

/// Navigating into a plain string (not valid JSON) returns an error.
#[test]
fn extract_json_path_string_not_json() {
    let json = r#"{"name": "plain text"}"#;
    let err = extract_json_path(json, "name.sub").unwrap_err();
    assert!(
        err.message.contains("not valid JSON"),
        "error should mention invalid JSON: {}",
        err.message
    );
}

/// Navigate into an object nested inside a string, then into an array.
#[test]
fn extract_json_path_multi_level() {
    let inner_inner = serde_json::json!({"rows": [{"x": 10}]});
    let inner = serde_json::json!({"payload": inner_inner.to_string()});
    let outer = serde_json::json!({"wrapper": inner.to_string()});

    let out = extract_json_path(&outer.to_string(), "wrapper.payload.rows").unwrap();
    let arr: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["x"], 10);
}

/// End-to-end: extract from Splunk-shaped wrapper and ingest into Hyper.
#[test]
fn extract_json_path_then_ingest() {
    let te = TestEngine::new_ephemeral();
    let inner = serde_json::json!({
        "status": "success",
        "query_result": {
            "results": [
                {"id": 1, "msg": "hello"},
                {"id": 2, "msg": "world"},
            ]
        }
    });
    let wrapper = serde_json::json!({
        "content": [{"type": "text", "text": inner.to_string()}]
    });

    let extracted =
        extract_json_path(&wrapper.to_string(), "content.0.text.query_result.results").unwrap();

    let opts = IngestOptions {
        table: "splunk_data".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let result = ingest_json(&te.engine, &extracted, &opts).unwrap();
    assert_eq!(result.rows, 2);

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM splunk_data ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["msg"], "hello");
    assert_eq!(rows[1]["msg"], "world");
}

// ──────────────────────────────────────────────────────────────────
// merge mode
// ──────────────────────────────────────────────────────────────────

/// Initial 3 rows, then merge 2 overlapping (update) + 1 new (insert).
/// Final shape: 4 rows; updated rows show the new values.
#[test]
fn ingest_json_merge_basic() {
    let te = TestEngine::new_ephemeral();
    let initial = r#"[
        {"id": 1, "name": "Alice"},
        {"id": 2, "name": "Bob"},
        {"id": 3, "name": "Carol"}
    ]"#;
    let updates = r#"[
        {"id": 2, "name": "Bob Jr."},
        {"id": 3, "name": "Carol Updated"},
        {"id": 4, "name": "Dave"}
    ]"#;

    let opts_replace = IngestOptions {
        table: "users".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, initial, &opts_replace).unwrap();

    let opts_merge = IngestOptions {
        table: "users".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: None,
    };
    let merge_result = ingest_json(&te.engine, updates, &opts_merge).unwrap();
    // No new columns in this merge → schema_changed must remain false so
    // the server handler skips the resource-list-changed broadcast.
    assert!(
        !merge_result.stats.schema_changed,
        "schema_changed must be false for a row-only merge"
    );

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM users ORDER BY id")
        .unwrap();
    assert_eq!(
        rows.len(),
        4,
        "expected 3 originals minus 2 replaced + 3 merged = 4"
    );
    assert_eq!(rows[0]["name"], "Alice"); // unchanged
    assert_eq!(rows[1]["name"], "Bob Jr."); // updated
    assert_eq!(rows[2]["name"], "Carol Updated"); // updated
    assert_eq!(rows[3]["name"], "Dave"); // inserted
}

/// Initial table has columns (id, name); merge file adds `host`.
/// Verify ALTER TABLE auto-fires, post-merge schema includes `host`,
/// and old rows have `host = NULL`.
#[test]
fn ingest_json_merge_adds_new_column() {
    let te = TestEngine::new_ephemeral();
    let initial = r#"[{"id": 1, "name": "Alice"}, {"id": 2, "name": "Bob"}]"#;
    let with_host = r#"[
        {"id": 2, "name": "Bob", "host": "host-2"},
        {"id": 3, "name": "Carol", "host": "host-3"}
    ]"#;

    let opts_replace = IngestOptions {
        table: "t".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, initial, &opts_replace).unwrap();

    let opts_merge = IngestOptions {
        table: "t".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: None,
    };
    let merge_result = ingest_json(&te.engine, with_host, &opts_merge).unwrap();
    // ALTER TABLE fired → schema_changed must be true so the server
    // handler issues a resource-list-changed broadcast and clients
    // re-fetch their schema cache.
    assert!(
        merge_result.stats.schema_changed,
        "schema_changed must be true when merge ALTERs the target"
    );

    // Schema now includes host.
    let cols = te.engine.column_metadata("t").unwrap();
    assert!(
        cols.iter().any(|c| c.name == "host"),
        "expected `host` column added by ALTER TABLE; got {:?}",
        cols.iter().map(|c| &c.name).collect::<Vec<_>>()
    );

    // id=1 (untouched) should have host=NULL; id=2,3 should have values.
    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM t ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows[0]["host"].is_null(), "old row's host must be NULL");
    assert_eq!(rows[1]["host"], "host-2");
    assert_eq!(rows[2]["host"], "host-3");
}

/// Merging into a non-existent table degenerates to a rename: the temp
/// table becomes the target and rows are loaded as-if by replace.
#[test]
fn ingest_json_merge_target_does_not_exist() {
    let te = TestEngine::new_ephemeral();
    let data = r#"[{"id": 1, "name": "Alice"}]"#;
    let opts_merge = IngestOptions {
        table: "fresh".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: None,
    };
    let result = ingest_json(&te.engine, data, &opts_merge).unwrap();
    assert_eq!(result.rows, 1);
    // Target was just created from scratch via the rename short-circuit;
    // by definition this is a "shape changed" event, so notify clients.
    assert!(
        result.stats.schema_changed,
        "schema_changed must be true when merge creates a fresh target"
    );

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM fresh")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Alice");
}

/// `mode=merge` with no `merge_key` returns `InvalidArgument` from the
/// ingest layer (the server-handler validation is exercised separately
/// at the tool boundary).
#[test]
fn ingest_json_merge_missing_key_param() {
    let te = TestEngine::new_ephemeral();
    let data = r#"[{"id": 1}]"#;
    let opts = IngestOptions {
        table: "t".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let err = ingest_json(&te.engine, data, &opts).unwrap_err();
    assert!(
        err.message.to_lowercase().contains("merge_key"),
        "error must mention merge_key; got: {}",
        err.message
    );
}

/// Merge with a key column that the *target* doesn't have. Caused by
/// a typo in the merge_key parameter or a target schema that diverges
/// from the file. Should raise a clear error and leave the target
/// untouched.
#[test]
fn ingest_json_merge_key_not_in_target() {
    let te = TestEngine::new_ephemeral();
    // Target has only `id`. Merge attempts to key on `not_a_col`.
    let opts_replace = IngestOptions {
        table: "t".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, r#"[{"id": 1}]"#, &opts_replace).unwrap();

    let opts_merge = IngestOptions {
        table: "t".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["not_a_col".into()]),
        target_db: None,
    };
    let err = ingest_json(&te.engine, r#"[{"id": 2, "not_a_col": "x"}]"#, &opts_merge).unwrap_err();
    assert!(
        err.message.contains("not_a_col"),
        "error must name the missing column; got: {}",
        err.message
    );
    assert!(err.message.to_lowercase().contains("not in target"));

    // Target unchanged.
    let count: i64 = te
        .engine
        .connection()
        .execute_scalar_query("SELECT COUNT(*) FROM t")
        .unwrap()
        .unwrap();
    assert_eq!(count, 1);
}

/// Merge where the target's `id` is BIGINT (forced) but the incoming
/// JSON file's id parses as TEXT — a real source of confusion in
/// practice. Reject with a clear error rather than silently coercing.
#[test]
fn ingest_json_merge_key_type_mismatch() {
    let te = TestEngine::new_ephemeral();
    // Force `id` to BIGINT explicitly.
    let mut so = serde_json::Map::new();
    so.insert("id".into(), serde_json::json!("BIGINT"));
    let opts_replace = IngestOptions {
        table: "t".into(),
        mode: "replace".into(),
        schema_override: Some(so),
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, r#"[{"id": 1, "name": "a"}]"#, &opts_replace).unwrap();

    // Incoming has id as quoted string, no override → inferred TEXT.
    let opts_merge = IngestOptions {
        table: "t".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: None,
    };
    let err = ingest_json(&te.engine, r#"[{"id": "1", "name": "a"}]"#, &opts_merge).unwrap_err();
    assert!(
        err.message.to_lowercase().contains("type mismatch"),
        "error must mention type mismatch; got: {}",
        err.message
    );
}

/// Same shape as the key-type-mismatch test but for a non-key column.
/// We still reject because silently coercing risks data loss.
#[test]
fn ingest_json_merge_existing_column_type_mismatch() {
    let te = TestEngine::new_ephemeral();
    // Target has score as DOUBLE PRECISION.
    let mut so = serde_json::Map::new();
    so.insert("score".into(), serde_json::json!("DOUBLE PRECISION"));
    let opts_replace = IngestOptions {
        table: "t".into(),
        mode: "replace".into(),
        schema_override: Some(so),
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, r#"[{"id": 1, "score": 99.5}]"#, &opts_replace).unwrap();

    // Incoming has score as quoted text.
    let opts_merge = IngestOptions {
        table: "t".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: None,
    };
    let err = ingest_json(
        &te.engine,
        r#"[{"id": 1, "score": "not a number"}]"#,
        &opts_merge,
    )
    .unwrap_err();
    assert!(
        err.message.to_lowercase().contains("type mismatch"),
        "error must mention type mismatch; got: {}",
        err.message
    );
}

/// On any merge failure, the temp `__hyperdb_merge_*` table must be
/// dropped — leaving stragglers in the workspace would degrade the
/// describe/list experience over time.
#[test]
fn ingest_json_merge_no_orphan_tmp_on_failure() {
    let te = TestEngine::new_ephemeral();
    // Force a key-not-in-target failure (cheap, deterministic).
    let opts_replace = IngestOptions {
        table: "t".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, r#"[{"id": 1}]"#, &opts_replace).unwrap();

    let opts_merge = IngestOptions {
        table: "t".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["bogus_key".into()]),
        target_db: None,
    };
    let _ = ingest_json(&te.engine, r#"[{"id": 2, "bogus_key": "x"}]"#, &opts_merge);

    // Look for any leftover `__hyperdb_merge_*` table.
    let table_rows = te
        .engine
        .execute_query_to_json("SELECT table_name FROM \"$catalog\".\"public\".\"$tables\"")
        .or_else(|_| {
            // Fallback: if `$catalog` isn't queryable, use describe_tables (which the
            // engine exposes for the same purpose).
            te.engine.describe_tables().map(|tables| {
                tables
                    .into_iter()
                    .map(|v| serde_json::json!({ "table_name": v["name"] }))
                    .collect::<Vec<_>>()
            })
        })
        .unwrap();
    let has_orphan = table_rows.iter().any(|v| {
        v["table_name"]
            .as_str()
            .is_some_and(|s| s.starts_with("__hyperdb_merge_"))
    });
    assert!(
        !has_orphan,
        "merge failure must clean up its temp table; found orphan in: {table_rows:?}"
    );
}

/// Composite merge_key — a row matches only when *every* key column
/// agrees. Tests that the SQL builder emits `t."a" = s."a" AND
/// t."b" = s."b"` rather than just one clause, and that
/// non-matching key tuples insert as new rows.
#[test]
fn ingest_json_merge_multi_key() {
    let te = TestEngine::new_ephemeral();
    // Initial: 4 rows keyed by (region, year).
    let initial = r#"[
        {"region": "us", "year": 2025, "amount": 100},
        {"region": "us", "year": 2026, "amount": 200},
        {"region": "eu", "year": 2025, "amount": 300},
        {"region": "eu", "year": 2026, "amount": 400}
    ]"#;
    let opts_replace = IngestOptions {
        table: "sales".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, initial, &opts_replace).unwrap();

    // Merge: us/2026 updates (amount→999); eu/2027 is new; us/2025 stays untouched.
    let updates = r#"[
        {"region": "us", "year": 2026, "amount": 999},
        {"region": "eu", "year": 2027, "amount": 500}
    ]"#;
    let opts_merge = IngestOptions {
        table: "sales".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["region".into(), "year".into()]),
        target_db: None,
    };
    ingest_json(&te.engine, updates, &opts_merge).unwrap();

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM sales ORDER BY region, year")
        .unwrap();
    assert_eq!(rows.len(), 5, "4 initial - 1 replaced + 2 new = 5");
    // eu/2025 untouched
    assert_eq!(rows[0]["region"], "eu");
    assert_eq!(rows[0]["year"], 2025);
    assert_eq!(rows[0]["amount"], 300);
    // eu/2026 untouched (only the (us,2026) tuple matched, not (eu,2026))
    assert_eq!(rows[1]["amount"], 400);
    // eu/2027 inserted
    assert_eq!(rows[2]["year"], 2027);
    assert_eq!(rows[2]["amount"], 500);
    // us/2025 untouched
    assert_eq!(rows[3]["region"], "us");
    assert_eq!(rows[3]["year"], 2025);
    assert_eq!(rows[3]["amount"], 100);
    // us/2026 updated
    assert_eq!(rows[4]["amount"], 999);
}

/// Type canonicalization round-trip: `INT` and `INTEGER` are aliases
/// in `map_hyper_type`, and Hyper's catalog canonicalizes `INT` →
/// `INTEGER` on storage. Without `types_compatible`, the merge would
/// false-reject this round-trip with a spurious "type mismatch"
/// even though the types are semantically identical. This test pins
/// the alias behavior so future refactors don't regress it.
#[test]
fn ingest_json_merge_type_canonicalization_does_not_false_reject() {
    let te = TestEngine::new_ephemeral();

    // Force the target's `id` column to be created from the `"INT"` user
    // string. After `CREATE TABLE`, Hyper canonicalizes to `"INTEGER"`
    // in the catalog — that's the canonicalization we need to absorb.
    let mut so_int = serde_json::Map::new();
    so_int.insert("id".into(), serde_json::json!("INT"));
    let opts_replace = IngestOptions {
        table: "t".into(),
        mode: "replace".into(),
        schema_override: Some(so_int),
        merge_key: None,
        target_db: None,
    };
    ingest_json(&te.engine, r#"[{"id": 1, "name": "a"}]"#, &opts_replace).unwrap();

    // Sanity: confirm the catalog canonicalized to INTEGER. (If this
    // assertion ever fails, Hyper's behavior changed, and the test
    // becomes vacuous — but the round-trip below still has value.)
    let cols = te.engine.column_metadata("t").unwrap();
    let id_col = cols.iter().find(|c| c.name == "id").unwrap();
    assert!(
        id_col.hyper_type.eq_ignore_ascii_case("INTEGER"),
        "expected canonicalized 'INTEGER' (or 'integer'); got {:?}",
        id_col.hyper_type
    );

    // Now merge with the same logical type — but force the *override*
    // to use the `INTEGER` alias to drive the comparison. (Without an
    // override, JSON's number inferrer would emit BIGINT for this
    // value and we'd test something different.)
    let mut so_integer = serde_json::Map::new();
    so_integer.insert("id".into(), serde_json::json!("INTEGER"));
    let opts_merge = IngestOptions {
        table: "t".into(),
        mode: "merge".into(),
        schema_override: Some(so_integer),
        merge_key: Some(vec!["id".into()]),
        target_db: None,
    };
    let result = ingest_json(
        &te.engine,
        r#"[{"id": 1, "name": "updated"}, {"id": 2, "name": "new"}]"#,
        &opts_merge,
    );
    // Critical: the merge must succeed. A naive string comparison would
    // reject because target.hyper_type may be "INTEGER" while incoming
    // schema_override carries "INTEGER" verbatim — and Hyper might
    // store one as lower-cased. types_compatible normalizes both.
    assert!(
        result.is_ok(),
        "merge must not false-reject equivalent types; got: {:?}",
        result.err()
    );

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM t ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "updated");
    assert_eq!(rows[1]["name"], "new");
}

/// Smoke test for merge mode against inline CSV — `merge_via_temp_table`
/// is format-agnostic, but `ingest_csv` carries its own merge branch and
/// the symmetry deserves at least one test. Initial 3 rows; merge 2
/// overlapping (update) + 1 new (insert) → final 4.
#[test]
fn ingest_csv_merge_basic() {
    let te = TestEngine::new_ephemeral();
    let initial = "id,name\n1,Alice\n2,Bob\n3,Carol\n";
    let updates = "id,name\n2,Bob Jr.\n3,Carol Updated\n4,Dave\n";

    let opts_replace = IngestOptions {
        table: "users_csv".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    ingest_csv(&te.engine, initial, &opts_replace).unwrap();

    let opts_merge = IngestOptions {
        table: "users_csv".into(),
        mode: "merge".into(),
        schema_override: None,
        merge_key: Some(vec!["id".into()]),
        target_db: None,
    };
    let merge_result = ingest_csv(&te.engine, updates, &opts_merge).unwrap();
    assert!(
        !merge_result.stats.schema_changed,
        "row-only merge must leave schema_changed false"
    );

    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM users_csv ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["name"], "Alice"); // unchanged
    assert_eq!(rows[1]["name"], "Bob Jr."); // updated
    assert_eq!(rows[2]["name"], "Carol Updated"); // updated
    assert_eq!(rows[3]["name"], "Dave"); // inserted
}
