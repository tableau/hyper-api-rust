// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the export module: verifying each output format (CSV, Parquet,
//! Hyper) produces valid files with correct row counts, and that both
//! table-based and query-based exports work.

mod common;
use common::TestEngine;
use hyperdb_mcp::error::ErrorCode;
use hyperdb_mcp::export::{export_to_file, ExportOptions};

/// Create a small test table with mixed types (INT, TEXT, DOUBLE) and two rows.
fn setup_test_table(te: &TestEngine) {
    te.engine
        .execute_command(
            "CREATE TABLE test_export (id INT NOT NULL, name TEXT, val DOUBLE PRECISION)",
        )
        .unwrap();
    te.engine
        .execute_command("INSERT INTO test_export VALUES (1, 'Alice', 10.5)")
        .unwrap();
    te.engine
        .execute_command("INSERT INTO test_export VALUES (2, 'Bob', 20.3)")
        .unwrap();
}

/// Export an entire table to CSV using the table name (no SQL). Verify row
/// count matches and the file contains expected string values.
#[test]
fn export_csv_from_table() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.csv");
    let path_str = path.to_str().unwrap();
    let opts = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: path_str.into(),
        format: "csv".into(),
        overwrite: true,
        format_options: None,
    };
    let result = export_to_file(&te.engine, &opts).unwrap();
    assert_eq!(result.rows, 2);

    let contents = std::fs::read_to_string(path_str).unwrap();
    assert!(contents.contains("Alice"));
    assert!(contents.contains("Bob"));
}

/// Export filtered query results to CSV. Verifies that only the matching row
/// is exported when a WHERE clause limits the result set.
#[test]
fn export_csv_from_query() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("query_export.csv");
    let path_str = path.to_str().unwrap();
    let opts = ExportOptions {
        sql: Some("SELECT name FROM test_export WHERE id = 1".into()),
        table: None,
        path: path_str.into(),
        format: "csv".into(),
        overwrite: true,
        format_options: None,
    };
    let result = export_to_file(&te.engine, &opts).unwrap();
    assert_eq!(result.rows, 1);
}

/// Export an entire table to Parquet. Verify the row count and that the
/// output file is non-empty (valid Parquet files have a magic footer).
#[test]
fn export_parquet_from_table() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.parquet");
    let path_str = path.to_str().unwrap();
    let opts = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: path_str.into(),
        format: "parquet".into(),
        overwrite: true,
        format_options: None,
    };
    let result = export_to_file(&te.engine, &opts).unwrap();
    assert_eq!(result.rows, 2);
    assert!(std::fs::metadata(path_str).unwrap().len() > 0);
}

/// Export as .hyper by copying the workspace file. Verify the output is
/// a non-empty file (valid Hyper database file openable in Tableau Desktop).
#[test]
fn export_hyper_copies_workspace() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("export.hyper");
    let path_str = path.to_str().unwrap();
    let opts = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: path_str.into(),
        format: "hyper".into(),
        overwrite: true,
        format_options: None,
    };
    let _result = export_to_file(&te.engine, &opts).unwrap();
    assert!(std::fs::metadata(path_str).unwrap().len() > 0);
}

/// `format = "hyper"` is a whole-workspace file copy and must succeed even
/// when neither `sql` nor `table` is provided — the row-oriented
/// SQL-resolution check should not apply. Regression test for a dispatcher
/// bug where `export` erroneously returned "Either sql or table must be
/// provided" for a bare `{path, format: "hyper"}` call.
#[test]
fn export_hyper_requires_no_sql_or_table() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare_export.hyper");
    let path_str = path.to_str().unwrap();
    let opts = ExportOptions {
        sql: None,
        table: None,
        path: path_str.into(),
        format: "hyper".into(),
        overwrite: true,
        format_options: None,
    };
    let result = export_to_file(&te.engine, &opts)
        .expect("hyper format must accept a bare (path, format) call without sql or table");
    assert_eq!(result.rows, 0);
    assert_eq!(result.stats.format, "hyper");
    assert_eq!(result.stats.output_path, path_str);
    assert!(std::fs::metadata(path_str).unwrap().len() > 0);
}

/// Row-oriented formats (csv, parquet, `arrow_ipc`) still require `sql` or
/// `table`. Make sure the hyper-format bypass didn't accidentally weaken
/// the check for the formats that genuinely need it.
#[test]
fn export_csv_without_sql_or_table_errors() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.csv");
    let path_str = path.to_str().unwrap();
    let opts = ExportOptions {
        sql: None,
        table: None,
        path: path_str.into(),
        format: "csv".into(),
        overwrite: true,
        format_options: None,
    };
    let Err(err) = export_to_file(&te.engine, &opts) else {
        panic!("csv export must reject calls with no sql and no table")
    };
    assert!(
        err.message.contains("sql") && err.message.contains("table"),
        "expected sql/table error, got: {}",
        err.message
    );
}

/// `overwrite = false` must refuse to clobber an existing target file and
/// leave its contents untouched. Applies to every format — verified here
/// with CSV (cheapest) and `hyper` (the file-copy path that motivated the
/// flag).
#[test]
fn export_overwrite_false_rejects_existing_file() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();

    // --- CSV path ---
    let csv_path = dir.path().join("exists.csv");
    let sentinel_csv = b"SENTINEL_ORIGINAL_CONTENTS\n";
    std::fs::write(&csv_path, sentinel_csv).unwrap();

    let Err(err) = export_to_file(
        &te.engine,
        &ExportOptions {
            sql: None,
            table: Some("test_export".into()),
            path: csv_path.to_str().unwrap().into(),
            format: "csv".into(),
            overwrite: false,
            format_options: None,
        },
    ) else {
        panic!("export with overwrite=false must error when target exists")
    };
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    assert!(
        err.message.to_lowercase().contains("overwrite"),
        "expected overwrite hint in error message, got: {}",
        err.message
    );
    let after = std::fs::read(&csv_path).unwrap();
    assert_eq!(
        after, sentinel_csv,
        "existing file must be untouched when overwrite=false"
    );

    // --- Hyper path (the file-copy format the flag was motivated by) ---
    let hyper_path = dir.path().join("exists.hyper");
    let sentinel_hyper = b"SENTINEL_NOT_A_REAL_HYPER_FILE";
    std::fs::write(&hyper_path, sentinel_hyper).unwrap();

    let Err(err) = export_to_file(
        &te.engine,
        &ExportOptions {
            sql: None,
            table: None,
            path: hyper_path.to_str().unwrap().into(),
            format: "hyper".into(),
            overwrite: false,
            format_options: None,
        },
    ) else {
        panic!("hyper export with overwrite=false must error when target exists")
    };
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    let after = std::fs::read(&hyper_path).unwrap();
    assert_eq!(
        after, sentinel_hyper,
        "existing .hyper file must be untouched when overwrite=false"
    );
}

/// `overwrite = false` must still allow writing to a *new* path — the
/// check only fires when the target already exists.
#[test]
fn export_overwrite_false_allows_new_path() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.csv");
    let opts = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: path.to_str().unwrap().into(),
        format: "csv".into(),
        overwrite: false,
        format_options: None,
    };
    let result = export_to_file(&te.engine, &opts)
        .expect("overwrite=false must succeed when target doesn't exist yet");
    assert_eq!(result.rows, 2);
    assert!(std::fs::metadata(&path).unwrap().len() > 0);
}

/// `overwrite = true` replaces an existing file — this is both the default
/// and the long-standing behavior from before the flag existed.
#[test]
fn export_overwrite_true_replaces_existing_file() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replace.csv");
    std::fs::write(&path, b"SENTINEL_OLD").unwrap();

    let opts = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: path.to_str().unwrap().into(),
        format: "csv".into(),
        overwrite: true,
        format_options: None,
    };
    let result = export_to_file(&te.engine, &opts)
        .expect("overwrite=true must succeed even when target already exists");
    assert_eq!(result.rows, 2);
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        !contents.contains("SENTINEL_OLD"),
        "sentinel not replaced — file contents were: {contents}"
    );
    assert!(contents.contains("Alice") && contents.contains("Bob"));
}

/// Iceberg round-trip: export a table to an Iceberg directory, then read
/// it back with `load_iceberg` and verify the row count plus payload
/// match. This is the best integration signal we can get without
/// shipping Iceberg fixture data — `export` is our producer, `load_iceberg`
/// is our consumer, and hyperd sits on both sides.
#[test]
fn iceberg_export_round_trips_through_load_iceberg() {
    use hyperdb_mcp::lakehouse::{ingest_iceberg_table, IcebergIngestOptions};

    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);

    let dir = tempfile::tempdir().unwrap();
    let iceberg_path = dir.path().join("export_iceberg");
    let iceberg_str = iceberg_path.to_str().unwrap();

    // Export.
    let export_opts = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: iceberg_str.into(),
        format: "iceberg".into(),
        overwrite: true,
        format_options: None,
    };
    let export_result = export_to_file(&te.engine, &export_opts).unwrap();
    assert_eq!(export_result.rows, 2);

    // The output must be a directory with a `metadata/` subdir — the
    // shape `load_iceberg` expects.
    assert!(
        iceberg_path.is_dir(),
        "iceberg export must produce a directory"
    );
    assert!(
        iceberg_path.join("metadata").is_dir(),
        "iceberg export must produce a metadata/ subdir"
    );

    // Reload into a fresh table and verify rows.
    let ingest_opts = IcebergIngestOptions {
        table: "test_reloaded".into(),
        mode: "replace".into(),
        metadata_filename: None,
        version_as_of: None,
    };
    let ingest_result = ingest_iceberg_table(&te.engine, iceberg_str, &ingest_opts).unwrap();
    assert_eq!(ingest_result.rows, 2);

    // The reported schema must list all three source columns. The initial
    // implementation queried `information_schema.columns` (which hyperd
    // does not expose) and silently fell back to an empty schema — this
    // assertion guards against that regression.
    assert_eq!(
        ingest_result.schema.len(),
        3,
        "load_iceberg should report all 3 columns from test_export, got: {:?}",
        ingest_result.schema
    );
    let names: Vec<&str> = ingest_result
        .schema
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(names.contains(&"id"));
    assert!(names.contains(&"name"));
    assert!(names.contains(&"val"));

    // Verify the reloaded payload matches the source.
    let rows = te
        .engine
        .execute_query_to_json("SELECT id, name, val FROM test_reloaded ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], 1);
    assert_eq!(rows[0]["name"], "Alice");
    assert_eq!(rows[1]["id"], 2);
    assert_eq!(rows[1]["name"], "Bob");

    // And a full EXCEPT-based equality check: no row in the source that
    // isn't present in the reload, and vice-versa.
    let src_minus_reload = te
        .engine
        .execute_query_to_json(
            "SELECT COUNT(*) AS n FROM (SELECT * FROM test_export EXCEPT SELECT * FROM test_reloaded) d",
        )
        .unwrap();
    assert_eq!(src_minus_reload[0]["n"], 0);
    let reload_minus_src = te
        .engine
        .execute_query_to_json(
            "SELECT COUNT(*) AS n FROM (SELECT * FROM test_reloaded EXCEPT SELECT * FROM test_export) d",
        )
        .unwrap();
    assert_eq!(reload_minus_src[0]["n"], 0);
}

/// Overwriting an existing Iceberg directory with a fresh export must
/// succeed and replace the contents (not append or merge into the old
/// layout). The old directory is removed and a new Iceberg table takes
/// its place.
#[test]
fn iceberg_export_overwrite_replaces_directory() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);

    let dir = tempfile::tempdir().unwrap();
    let iceberg_path = dir.path().join("export_iceberg_overwrite");
    let iceberg_str = iceberg_path.to_str().unwrap();

    // First export: two rows.
    let opts_first = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: iceberg_str.into(),
        format: "iceberg".into(),
        overwrite: true,
        format_options: None,
    };
    let r1 = export_to_file(&te.engine, &opts_first).unwrap();
    assert_eq!(r1.rows, 2);

    // Second export over the same directory, filtered to one row.
    let opts_second = ExportOptions {
        sql: Some("SELECT * FROM test_export WHERE id = 1".into()),
        table: None,
        path: iceberg_str.into(),
        format: "iceberg".into(),
        overwrite: true,
        format_options: None,
    };
    let r2 = export_to_file(&te.engine, &opts_second).unwrap();
    assert_eq!(r2.rows, 1);

    // Reload and confirm only the filtered row survived — the original
    // 2-row Iceberg table was replaced, not augmented.
    use hyperdb_mcp::lakehouse::{ingest_iceberg_table, IcebergIngestOptions};
    let ingest = ingest_iceberg_table(
        &te.engine,
        iceberg_str,
        &IcebergIngestOptions {
            table: "t".into(),
            mode: "replace".into(),
            metadata_filename: None,
            version_as_of: None,
        },
    )
    .unwrap();
    assert_eq!(ingest.rows, 1);
}

/// overwrite=false must refuse to write into an existing Iceberg
/// directory and return `PermissionDenied` before issuing any SQL. Same
/// contract as the file-based formats.
#[test]
fn iceberg_export_refuses_overwrite_when_disabled() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);

    let dir = tempfile::tempdir().unwrap();
    let iceberg_path = dir.path().join("export_iceberg_no_overwrite");
    let iceberg_str = iceberg_path.to_str().unwrap();

    // First export succeeds.
    let opts_first = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: iceberg_str.into(),
        format: "iceberg".into(),
        overwrite: true,
        format_options: None,
    };
    export_to_file(&te.engine, &opts_first).unwrap();

    // Second with overwrite=false must refuse.
    let opts_second = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: iceberg_str.into(),
        format: "iceberg".into(),
        overwrite: false,
        format_options: None,
    };
    let err = export_to_file(&te.engine, &opts_second).err().unwrap();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
}

/// Parquet round-trip: export a table via the native `COPY TO` parquet
/// writer, re-ingest via `load_file`, verify row counts, payload, and
/// — critically — that exact column types are preserved. The old
/// JSON-mediated Rust export would downgrade NUMERIC/DATE/etc. to
/// generic TEXT or FLOAT64 because JSON-ification lost the type info.
#[test]
fn parquet_export_round_trips_through_load_file() {
    use hyperdb_mcp::ingest::IngestOptions;
    use hyperdb_mcp::ingest_arrow::ingest_parquet_file;

    let te = TestEngine::new_ephemeral();
    // A table whose columns include every type the old JSON-based
    // exporter would have degraded: NUMERIC with scale, DATE, plus
    // plain INT/TEXT/DOUBLE. The round-trip must preserve all of them.
    te.engine
        .execute_command(
            "CREATE TABLE pq_export_src ( \
               id INT NOT NULL, \
               name TEXT NOT NULL, \
               amount NUMERIC(15, 2) NOT NULL, \
               val DOUBLE PRECISION, \
               d DATE NOT NULL \
             )",
        )
        .unwrap();
    te.engine
        .execute_command(
            "INSERT INTO pq_export_src VALUES \
               (1, 'Alice', 123.45, 10.5, DATE '2024-01-15'), \
               (2, 'Bob',  -678.90, 20.3, DATE '2025-07-04'), \
               (3, 'Cara',    0.00, NULL,  DATE '1999-12-31')",
        )
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.parquet");
    let path_str = path.to_str().unwrap();

    // Export.
    let export_result = export_to_file(
        &te.engine,
        &ExportOptions {
            sql: None,
            table: Some("pq_export_src".into()),
            path: path_str.into(),
            format: "parquet".into(),
            overwrite: true,
            format_options: None,
        },
    )
    .unwrap();
    assert_eq!(export_result.rows, 3);
    assert!(std::fs::metadata(path_str).unwrap().len() > 0);

    // Reload through our own parquet loader.
    let ingest_result = ingest_parquet_file(
        &te.engine,
        path_str,
        &IngestOptions {
            table: "pq_export_reloaded".into(),
            mode: "replace".into(),
            schema_override: None,
            merge_key: None,
            target_db: None,
        },
    )
    .unwrap();
    assert_eq!(ingest_result.rows, 3);

    // Type preservation — the big regression this test exists to catch.
    let col_type = |name: &str| -> String {
        ingest_result
            .schema
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.hyper_type.clone())
            .unwrap_or_default()
    };
    assert_eq!(col_type("id"), "INT");
    assert_eq!(col_type("name"), "TEXT");
    assert_eq!(col_type("amount"), "NUMERIC(15, 2)");
    assert_eq!(col_type("val"), "DOUBLE PRECISION");
    assert_eq!(col_type("d"), "DATE");

    // Row-level equality across every column. If the parquet path had
    // quietly downgraded types (say NUMERIC → DOUBLE) the EXCEPT would
    // still produce 0 if the conversion were lossless — so we also
    // compare the exact string form of the decimal to guard against
    // float drift.
    let src_minus_reload = te
        .engine
        .execute_query_to_json(
            "SELECT COUNT(*) AS n FROM (SELECT * FROM pq_export_src EXCEPT SELECT * FROM pq_export_reloaded) d",
        )
        .unwrap();
    assert_eq!(src_minus_reload[0]["n"], 0);
    let reload_minus_src = te
        .engine
        .execute_query_to_json(
            "SELECT COUNT(*) AS n FROM (SELECT * FROM pq_export_reloaded EXCEPT SELECT * FROM pq_export_src) d",
        )
        .unwrap();
    assert_eq!(reload_minus_src[0]["n"], 0);

    let rows = te
        .engine
        .execute_query_to_json(
            "SELECT amount::TEXT AS amount_str, d::TEXT AS d_str FROM pq_export_reloaded ORDER BY id",
        )
        .unwrap();
    assert_eq!(rows[0]["amount_str"], "123.45");
    assert_eq!(rows[1]["amount_str"], "-678.90");
    assert_eq!(rows[2]["amount_str"], "0.00");
    assert_eq!(rows[0]["d_str"], "2024-01-15");
}

/// Arrow IPC round-trip: export via `COPY TO ... format => 'arrowstream'`,
/// re-ingest via `load_file` (which auto-detects the Stream sub-format),
/// verify row counts and data. This pins the `arrowstream` format-string
/// against regression and confirms Stream-format exports are readable by
/// our own loader.
#[test]
fn arrow_ipc_export_round_trips_through_load_file() {
    use hyperdb_mcp::ingest::IngestOptions;
    use hyperdb_mcp::ingest_arrow::ingest_arrow_ipc_file;

    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.arrow");
    let path_str = path.to_str().unwrap();

    let export_result = export_to_file(
        &te.engine,
        &ExportOptions {
            sql: None,
            table: Some("test_export".into()),
            path: path_str.into(),
            format: "arrow_ipc".into(),
            overwrite: true,
            format_options: None,
        },
    )
    .unwrap();
    assert_eq!(export_result.rows, 2);
    assert!(std::fs::metadata(path_str).unwrap().len() > 0);

    // Reload through the Arrow IPC ingest path. It auto-detects the
    // Stream vs File sub-format, so an export producing Stream bytes
    // round-trips without extra conversion.
    let ingest_result = ingest_arrow_ipc_file(
        &te.engine,
        path_str,
        &IngestOptions {
            table: "arrow_reloaded".into(),
            mode: "replace".into(),
            schema_override: None,
            merge_key: None,
            target_db: None,
        },
    )
    .unwrap();
    assert_eq!(ingest_result.rows, 2);

    // Every reloaded row must match the source row-for-row.
    let src_minus_reload = te
        .engine
        .execute_query_to_json(
            "SELECT COUNT(*) AS n FROM (SELECT * FROM test_export EXCEPT SELECT * FROM arrow_reloaded) d",
        )
        .unwrap();
    assert_eq!(src_minus_reload[0]["n"], 0);
    let reload_minus_src = te
        .engine
        .execute_query_to_json(
            "SELECT COUNT(*) AS n FROM (SELECT * FROM arrow_reloaded EXCEPT SELECT * FROM test_export) d",
        )
        .unwrap();
    assert_eq!(reload_minus_src[0]["n"], 0);
}

/// `format_options` must actually reach hyperd. Export the same table
/// three times with different parquet `compression` values and confirm
/// the written file size differs — that's the cleanest proof the
/// option ended up in the `WITH (...)` clause rather than getting
/// silently dropped.
#[test]
fn parquet_export_honors_compression_override() {
    let te = TestEngine::new_ephemeral();

    // Generate something compressible — a wide string column with a
    // small alphabet. Snappy and ZSTD will both compress it well;
    // uncompressed will be much larger.
    te.engine
        .execute_command(
            "CREATE TABLE compression_src AS \
             SELECT i AS id, repeat('abcdefg', 200) AS payload \
             FROM generate_series(1, 20000) s(i)",
        )
        .unwrap();

    let dir = tempfile::tempdir().unwrap();

    let export_with = |compression: &str, name: &str| -> u64 {
        let path = dir.path().join(name);
        let mut opts = serde_json::Map::new();
        opts.insert(
            "codec".into(),
            serde_json::Value::String(compression.into()),
        );
        export_to_file(
            &te.engine,
            &ExportOptions {
                sql: None,
                table: Some("compression_src".into()),
                path: path.to_str().unwrap().into(),
                format: "parquet".into(),
                overwrite: true,
                format_options: Some(opts),
            },
        )
        .expect("parquet export with compression override should succeed");
        std::fs::metadata(&path).unwrap().len()
    };

    let uncompressed = export_with("uncompressed", "u.parquet");
    let zstd = export_with("zstd", "z.parquet");

    // ZSTD compresses the repetitive payload strictly smaller than
    // uncompressed. If the `codec` option were silently dropped both
    // files would use the same default codec and come out the same
    // size. (Parquet page-level dictionary encoding also compresses
    // before the codec runs, so the delta on this synthetic dataset is
    // modest — we only need a strict inequality here, not a big
    // ratio.)
    assert!(
        uncompressed > zstd,
        "expected zstd < uncompressed on a repetitive payload; \
         got uncompressed={uncompressed} zstd={zstd}"
    );
    // And a loose sanity floor: the two should differ by at least a
    // few kilobytes — rules out "both files are identical because the
    // option was dropped and silently fell back to the default codec."
    let delta = uncompressed.abs_diff(zstd);
    assert!(
        delta > 1024,
        "expected the codec override to make a meaningful size \
         difference (>1 KB); got delta={delta}"
    );
}

/// CSV `delimiter` override reaches hyperd too. Export with a tab
/// delimiter and confirm the output contains tabs rather than commas.
#[test]
fn csv_export_honors_delimiter_override() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tab.tsv");
    let path_str = path.to_str().unwrap();

    let mut opts = serde_json::Map::new();
    opts.insert("delimiter".into(), serde_json::Value::String("\t".into()));
    export_to_file(
        &te.engine,
        &ExportOptions {
            sql: None,
            table: Some("test_export".into()),
            path: path_str.into(),
            format: "csv".into(),
            overwrite: true,
            format_options: Some(opts),
        },
    )
    .unwrap();

    let contents = std::fs::read_to_string(path_str).unwrap();
    assert!(
        contents.contains('\t'),
        "CSV output must contain tabs when delimiter => '\\t'; got: {contents}"
    );
    // Sanity: the header row should no longer contain commas between
    // the field names.
    let header = contents.lines().next().unwrap();
    assert!(
        !header.contains(','),
        "header must not contain commas with a tab delimiter; got: {header}"
    );
}

/// Nonsense values in `format_options` must produce a clean error before
/// any SQL is issued to hyperd — not a SQL parse error or a successful
/// export with the option silently dropped. Also a guard against a
/// future attempt to slip injection through the key channel: even
/// though we properly quote string values, the renderer still rejects
/// key shapes that couldn't be valid option names.
#[test]
fn format_options_invalid_shapes_reject_cleanly() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.parquet");
    let path_str = path.to_str().unwrap().to_string();

    // Null value: rejected as non-scalar.
    let mut null_val = serde_json::Map::new();
    null_val.insert("compression".into(), serde_json::Value::Null);
    let err = export_to_file(
        &te.engine,
        &ExportOptions {
            sql: None,
            table: Some("test_export".into()),
            path: path_str.clone(),
            format: "parquet".into(),
            overwrite: true,
            format_options: Some(null_val),
        },
    )
    .err()
    .unwrap();
    assert!(err.message.to_lowercase().contains("compression"));

    // Bad key shape (injection attempt): rejected before SQL is built.
    let mut bad_key = serde_json::Map::new();
    bad_key.insert(
        "compression); DROP TABLE test_export --".into(),
        serde_json::Value::String("zstd".into()),
    );
    let err = export_to_file(
        &te.engine,
        &ExportOptions {
            sql: None,
            table: Some("test_export".into()),
            path: path_str.clone(),
            format: "parquet".into(),
            overwrite: true,
            format_options: Some(bad_key),
        },
    )
    .err()
    .unwrap();
    assert!(err.message.to_lowercase().contains("format_options key"));

    // And the target table must still exist — no injection landed.
    let rows = te
        .engine
        .execute_query_to_json("SELECT COUNT(*) AS n FROM test_export")
        .unwrap();
    assert_eq!(rows[0]["n"], 2);
}
