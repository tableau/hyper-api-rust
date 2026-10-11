// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for export to CSV, Parquet, Arrow IPC, Iceberg, and Hyper.
//!
//! Covers table-based and query-based exports, `format_options`, overwrite
//! handling, `source_db`, and type and constraint preservation.

mod common;
use common::TestEngine;
use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::error::ErrorCode;
use hyperdb_mcp::export::{ExportOptions, export_to_file};

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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
    };
    let result = export_to_file(&te.engine, &opts).unwrap();
    assert_eq!(result.rows, 1);
}

/// Export an entire table to Parquet. Verify the row count and that the
/// output file ends with the Parquet `PAR1` magic footer.
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
    };
    let result = export_to_file(&te.engine, &opts).unwrap();
    assert_eq!(result.rows, 2);
    assert!(std::fs::read(path_str).unwrap().ends_with(b"PAR1"));
}

/// A `table` value is ignored for `format = "hyper"`; the export still
/// writes a non-empty `.hyper` file.
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
    };
    let _result = export_to_file(&te.engine, &opts).unwrap();
    assert!(std::fs::metadata(path_str).unwrap().len() > 0);
}

/// `format = "hyper"` with `source_db = Some("persistent")` snapshots
/// the persistent attachment instead of the primary. Verifies the
/// snapshot contains only the source database's tables.
#[test]
fn export_hyper_with_source_db_snapshots_persistent() {
    let te = TestEngine::new_ephemeral();

    // Create a table in primary that should NOT appear in the snapshot,
    // and a different table in persistent that SHOULD.
    te.engine
        .execute_command("CREATE TABLE primary_only (n INT)")
        .unwrap();
    te.engine
        .execute_command("INSERT INTO primary_only VALUES (99)")
        .unwrap();
    te.engine
        .execute_command(
            "CREATE TABLE \"persistent\".\"public\".\"persisted\" (id INT, label TEXT)",
        )
        .unwrap();
    te.engine
        .execute_command("INSERT INTO \"persistent\".\"public\".\"persisted\" VALUES (1, 'a')")
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.hyper");
    let path_str = path.to_str().unwrap();

    let opts = ExportOptions {
        sql: None,
        table: None,
        path: path_str.into(),
        format: "hyper".into(),
        overwrite: true,
        protected_paths: Vec::new(),
        format_options: None,
        source_db: Some("persistent".into()),
    };
    export_to_file(&te.engine, &opts).expect("hyper export with source_db must succeed");

    // Verify the snapshot file contains the persistent table and NOT
    // the primary-only table by opening it as a fresh persistent
    // workspace and listing tables.
    let probe_engine = Engine::new_no_daemon(Some(path_str.into())).unwrap();
    let rows = probe_engine
        .execute_query_to_json(
            "SELECT tablename FROM \"persistent\".pg_catalog.pg_tables \
             WHERE schemaname = 'public' ORDER BY tablename",
        )
        .unwrap();
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            r.get("tablename")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect();
    assert!(
        names.contains(&"persisted".into()),
        "snapshot must contain 'persisted' from source_db; got {names:?}"
    );
    assert!(
        !names.contains(&"primary_only".into()),
        "snapshot must NOT contain 'primary_only' (lives in primary, not persistent); got {names:?}"
    );
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
    };
    let result = export_to_file(&te.engine, &opts)
        .expect("hyper format must accept a bare (path, format) call without sql or table");
    // The two rows `setup_test_table` inserted. This asserted 0 while the
    // export ran on `CREATE TABLE AS SELECT`, which reports no affected rows;
    // the constraint-preserving copy uses `INSERT ... SELECT` and reports the
    // count it actually wrote.
    assert_eq!(result.rows, 2);
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
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
            protected_paths: Vec::new(),
            format_options: None,
            source_db: None,
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
            protected_paths: Vec::new(),
            format_options: None,
            source_db: None,
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
    };
    let result = export_to_file(&te.engine, &opts)
        .expect("overwrite=false must succeed when target doesn't exist yet");
    assert_eq!(result.rows, 2);
    assert!(std::fs::metadata(&path).unwrap().len() > 0);
}

/// `overwrite = true` lets hyperd's `COPY ... TO` replace an existing file.
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
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

/// A `hyper` export never replaces a database the session has attached,
/// even with `overwrite`. The file is compared by identity, so another path
/// to it is refused too, and nothing is left beside it.
#[test]
fn export_hyper_over_an_attached_target_is_refused() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("already_attached.hyper");
    let path_str = path.to_str().unwrap();

    // Attach the target under an unrelated alias first, like a user calling
    // `attach_database` on a file they then ask to export a snapshot over.
    te.engine
        .execute_command(&format!(
            "CREATE DATABASE {}",
            hyperdb_api::escape_sql_path(path_str)
        ))
        .unwrap();
    te.engine
        .execute_command(&format!(
            "ATTACH DATABASE {} AS \"holder\"",
            hyperdb_api::escape_sql_path(path_str)
        ))
        .unwrap();
    // Metadata, not contents: on Windows hyperd holds the attached file
    // exclusively, so it cannot be read.
    let stamp = |p: &std::path::Path| {
        let meta = std::fs::metadata(p).unwrap();
        (meta.len(), meta.modified().unwrap())
    };
    let before = stamp(&path);

    for target in [
        path.clone(),
        dir.path().join(".").join("already_attached.hyper"),
    ] {
        let opts = ExportOptions {
            format: "hyper".into(),
            path: target.to_str().unwrap().into(),
            overwrite: true,
            protected_paths: vec![path.clone()],
            ..ExportOptions::default()
        };
        let err = export_to_file(&te.engine, &opts)
            .expect_err("exporting over an attached database must fail");
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
    }
    assert_eq!(stamp(&path), before);
    assert_eq!(dir_entries(dir.path()), ["already_attached.hyper"]);
}

/// Iceberg round-trip: export a table to an Iceberg directory, then read
/// it back with `load_iceberg` and verify the row count plus payload
/// match. This is the best integration signal we can get without
/// shipping Iceberg fixture data — `export` is our producer, `load_iceberg`
/// is our consumer, and hyperd sits on both sides.
#[test]
fn iceberg_export_round_trips_through_load_iceberg() {
    use hyperdb_mcp::lakehouse::{IcebergIngestOptions, ingest_iceberg_table};

    let mut te = TestEngine::new_ephemeral();
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
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
    let ingest_result = ingest_iceberg_table(&mut te.engine, iceberg_str, &ingest_opts).unwrap();
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
    let mut te = TestEngine::new_ephemeral();
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
    };
    let r2 = export_to_file(&te.engine, &opts_second).unwrap();
    assert_eq!(r2.rows, 1);

    // Reload and confirm only the filtered row survived — the original
    // 2-row Iceberg table was replaced, not augmented.
    use hyperdb_mcp::lakehouse::{IcebergIngestOptions, ingest_iceberg_table};
    let ingest = ingest_iceberg_table(
        &mut te.engine,
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
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
    };
    export_to_file(&te.engine, &opts_first).unwrap();

    // Second with overwrite=false must refuse.
    let opts_second = ExportOptions {
        sql: None,
        table: Some("test_export".into()),
        path: iceberg_str.into(),
        format: "iceberg".into(),
        overwrite: false,
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
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

    let mut te = TestEngine::new_ephemeral();
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
            protected_paths: Vec::new(),
            format_options: None,
            source_db: None,
        },
    )
    .unwrap();
    assert_eq!(export_result.rows, 3);
    assert!(std::fs::metadata(path_str).unwrap().len() > 0);

    // Reload through our own parquet loader.
    let ingest_result = ingest_parquet_file(
        &mut te.engine,
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

    let mut te = TestEngine::new_ephemeral();
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
            protected_paths: Vec::new(),
            format_options: None,
            source_db: None,
        },
    )
    .unwrap();
    assert_eq!(export_result.rows, 2);
    assert!(std::fs::metadata(path_str).unwrap().len() > 0);

    // Reload through the Arrow IPC ingest path. It auto-detects the
    // Stream vs File sub-format, so an export producing Stream bytes
    // round-trips without extra conversion.
    let ingest_result = ingest_arrow_ipc_file(
        &mut te.engine,
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
/// twice with different parquet `codec` values (`uncompressed`, `zstd`) and confirm
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
                protected_paths: Vec::new(),
                format_options: Some(opts),
                source_db: None,
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
            protected_paths: Vec::new(),
            format_options: Some(opts),
            source_db: None,
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
            protected_paths: Vec::new(),
            format_options: Some(null_val),
            source_db: None,
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
            protected_paths: Vec::new(),
            format_options: Some(bad_key),
            source_db: None,
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

/// The bug behind issue #127: the `.hyper` export used `CREATE TABLE AS
/// SELECT`, which infers the destination schema from the query's result
/// columns and therefore drops every constraint. A "backup" came back with
/// all columns nullable, no defaults and no keys. Assert the whole schema
/// now survives an export followed by a fresh attach of the exported file —
/// reading the constraints straight out of the exported database's own
/// `pg_catalog` rather than trusting the report.
#[test]
fn export_hyper_preserves_constraints() {
    let te = TestEngine::new_ephemeral();
    te.engine
        .execute_command(
            "CREATE TABLE constrained (id INT NOT NULL, code TEXT NOT NULL, \
             qty INT DEFAULT 7, note TEXT DEFAULT 'n/a', \
             label TEXT COLLATE \"en_US\", \
             ASSUMED PRIMARY KEY (id), ASSUMED UNIQUE (code))",
        )
        .unwrap();
    te.engine
        .execute_command("INSERT INTO constrained (id, code) VALUES (1, 'a')")
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("constrained.hyper");
    let path_str = path.to_str().unwrap();
    let result = export_to_file(
        &te.engine,
        &ExportOptions {
            sql: None,
            table: None,
            path: path_str.into(),
            format: "hyper".into(),
            overwrite: true,
            protected_paths: Vec::new(),
            format_options: None,
            source_db: None,
        },
    )
    .expect("hyper export must succeed");

    let report = result
        .schema_fidelity
        .expect("hyper export must report schema fidelity");
    assert_eq!(report.rows_copied, 1);
    assert_eq!(report.not_null_columns, 2);
    assert_eq!(report.default_columns, 2);
    assert_eq!(report.collated_columns, 1);
    assert_eq!(report.assumed_primary_keys, 1);
    assert_eq!(report.assumed_unique_constraints, 1);
    assert!(report.is_fully_preserved(), "{:?}", report.unpreserved);

    // Re-open the exported file on its own and read its real catalog.
    te.engine
        .execute_command(&format!(
            "ATTACH DATABASE {} AS \"verify\"",
            hyperdb_api::escape_sql_path(path_str)
        ))
        .unwrap();

    let column_rows = te
        .engine
        .execute_query_to_json(
            "SELECT a.attname AS name, CAST(a.attnotnull AS TEXT) AS nn, \
             CAST(a.atthasdef AS TEXT) AS hd \
             FROM \"verify\".pg_catalog.pg_attribute a \
             JOIN \"verify\".pg_catalog.pg_class c ON a.attrelid = c.oid \
             WHERE c.relname = 'constrained' AND a.attnum > 0 ORDER BY a.attnum",
        )
        .unwrap();
    assert_eq!(column_rows.len(), 5);
    assert_eq!(column_rows[0]["name"], "id");
    assert_eq!(column_rows[0]["nn"], "true");
    assert_eq!(column_rows[1]["nn"], "true");
    assert_eq!(column_rows[2]["hd"], "true");
    assert_eq!(column_rows[3]["hd"], "true");

    // Collation is part of the schema too, and was the one class an earlier
    // version of this copy dropped while still reporting full fidelity.
    let collation_rows = te
        .engine
        .execute_query_to_json(
            "SELECT a.attname AS name, coll.collname AS coll \
             FROM \"verify\".pg_catalog.pg_attribute a \
             JOIN \"verify\".pg_catalog.pg_class c ON a.attrelid = c.oid \
             LEFT JOIN \"verify\".pg_catalog.pg_collation coll ON coll.oid = a.attcollation \
             WHERE c.relname = 'constrained' AND a.attname = 'label'",
        )
        .unwrap();
    assert_eq!(collation_rows.len(), 1);
    assert_eq!(
        collation_rows[0]["coll"], "en_US",
        "exported file lost the column collation"
    );

    let constraint_rows = te
        .engine
        .execute_query_to_json(
            "SELECT CAST(con.contype AS TEXT) AS t, a.attname AS col \
             FROM \"verify\".pg_catalog.pg_constraint con \
             JOIN \"verify\".pg_catalog.pg_class c ON con.conrelid = c.oid, \
             unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
             JOIN \"verify\".pg_catalog.pg_attribute a \
               ON a.attrelid = con.conrelid AND a.attnum = k.attnum \
             WHERE c.relname = 'constrained' ORDER BY con.contype",
        )
        .unwrap();
    assert_eq!(constraint_rows.len(), 2);
    assert_eq!(constraint_rows[0]["t"], "p");
    assert_eq!(constraint_rows[0]["col"], "id");
    assert_eq!(constraint_rows[1]["t"], "u");
    assert_eq!(constraint_rows[1]["col"], "code");

    te.engine
        .execute_command("DETACH DATABASE \"verify\"")
        .unwrap();
}

/// Export options for `sql` written to `path` as `format`.
fn sql_export(sql: &str, path: &std::path::Path, format: &str) -> ExportOptions {
    ExportOptions {
        sql: Some(sql.into()),
        table: None,
        path: path.to_str().unwrap().into(),
        format: format.into(),
        overwrite: true,
        protected_paths: Vec::new(),
        format_options: None,
        source_db: None,
    }
}

/// Query mode splices the SQL into `COPY (...) TO <path>`, so SQL that
/// closes the parenthesis itself could name its own target and options.
/// It is refused, and neither target is written.
#[test]
fn export_refuses_sql_that_escapes_the_copy_wrapper() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    for format in ["csv", "iceberg"] {
        let path = dir.path().join(format!("export-{format}"));
        let escaped = dir.path().join(format!("escaped-{format}.csv"));
        let sql = format!(
            "SELECT * FROM test_export) TO '{}' WITH (format => 'csv') --",
            escaped.display()
        );
        let err = export_to_file(&te.engine, &sql_export(&sql, &path, format))
            .err()
            .unwrap_or_else(|| panic!("{format}: the escaping export must be refused"));
        assert_eq!(err.code, ErrorCode::SqlError, "{format}: {err:?}");
        assert!(
            !escaped.exists(),
            "{format}: the SQL's own target was written"
        );
        assert!(!path.exists(), "{format}: the export target was written");
    }
}

/// Query mode refuses a write, which Hyper would otherwise run (or reject
/// only by accident of the wrapper).
#[test]
fn export_refuses_write_sql() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refused.csv");
    for sql in [
        "DELETE FROM test_export",
        "WITH x AS (SELECT 1) DELETE FROM test_export",
    ] {
        let err = export_to_file(&te.engine, &sql_export(sql, &path, "csv"))
            .err()
            .unwrap_or_else(|| panic!("`{sql}` must be refused"));
        assert_eq!(err.code, ErrorCode::SqlError, "{sql}: {err:?}");
    }
    let rows = te
        .engine
        .execute_query_to_json("SELECT COUNT(*) AS n FROM test_export")
        .unwrap();
    assert_eq!(rows[0]["n"], 2, "the table must keep its rows");
    assert!(!path.exists());
}

/// A query that ends in a `--` comment still exports: the wrapper puts the
/// closing parenthesis on its own line.
#[test]
fn export_query_ending_in_a_line_comment() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    for format in ["csv", "iceberg"] {
        let path = dir.path().join(format!("commented-{format}"));
        let result = export_to_file(
            &te.engine,
            &sql_export("SELECT * FROM test_export -- both rows", &path, format),
        )
        .unwrap_or_else(|err| panic!("{format}: {err:?}"));
        assert_eq!(result.rows, 2, "{format}");
    }
}

/// The sorted names in `dir`.
fn dir_entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every file under `dir` with its contents, sorted, for byte-identity
/// checks across a failed export.
fn tree_snapshot(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for entry in std::fs::read_dir(&p).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                let rel = entry
                    .path()
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, std::fs::read(entry.path()).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// `overwrite=true` replaces only an Iceberg table: a directory holding
/// anything else, a `.hyper` file among it, or a regular file, is refused
/// and left as it was.
#[test]
fn iceberg_export_refuses_to_replace_what_is_not_an_iceberg_table() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();

    let sentinel = dir.path().join("documents");
    std::fs::create_dir(&sentinel).unwrap();
    std::fs::write(sentinel.join("notes.txt"), b"keep me").unwrap();
    let with_hyper = dir.path().join("databases");
    std::fs::create_dir_all(with_hyper.join("metadata")).unwrap();
    std::fs::write(with_hyper.join("metadata/v1.metadata.json"), b"{}").unwrap();
    std::fs::write(with_hyper.join("work.hyper"), b"Hyper\x08\0\0").unwrap();
    let file = dir.path().join("plain-file");
    std::fs::write(&file, b"keep me").unwrap();

    for target in [&sentinel, &with_hyper] {
        let before = tree_snapshot(target);
        let err = export_to_file(
            &te.engine,
            &sql_export("SELECT * FROM test_export", target, "iceberg"),
        )
        .expect_err("a non-Iceberg directory must not be replaced");
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
        assert!(
            err.message.contains("not an Iceberg table"),
            "{}",
            err.message
        );
        assert_eq!(tree_snapshot(target), before);
    }
    let err = export_to_file(
        &te.engine,
        &sql_export("SELECT * FROM test_export", &file, "iceberg"),
    )
    .expect_err("a regular file must not be replaced by an Iceberg export");
    assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
    assert_eq!(std::fs::read(&file).unwrap(), b"keep me");
    assert_eq!(
        dir_entries(dir.path()),
        ["databases", "documents", "plain-file"]
    );
}

/// An Iceberg replace that fails puts the previous table back byte for
/// byte and leaves no backup beside it. The cast fails while `COPY` runs,
/// after the query passed its parse check.
#[test]
fn failed_iceberg_replace_restores_the_previous_table() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("table");
    export_to_file(
        &te.engine,
        &sql_export("SELECT * FROM test_export", &target, "iceberg"),
    )
    .unwrap();
    let before = tree_snapshot(&target);

    let err = export_to_file(
        &te.engine,
        &sql_export(
            "SELECT CAST(name AS INT) AS n FROM test_export",
            &target,
            "iceberg",
        ),
    )
    .expect_err("the cast must fail at execution");
    // 22P02 (invalid integer text), hyperd's error rather than a refusal.
    assert_eq!(err.code, ErrorCode::SchemaMismatch, "{err:?}");
    assert_eq!(tree_snapshot(&target), before);
    assert_eq!(dir_entries(dir.path()), ["table"]);
}

/// `overwrite=true` replaces only a Hyper database file: anything else is
/// refused and untouched, while a real `.hyper` file is replaced.
#[test]
fn hyper_export_replaces_only_a_hyper_file() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let not_hyper = dir.path().join("report.hyper");
    std::fs::write(&not_hyper, b"Hype? not a database").unwrap();
    let a_dir = dir.path().join("dir.hyper");
    std::fs::create_dir(&a_dir).unwrap();

    let hyper_export = |path: &std::path::Path| ExportOptions {
        path: path.to_str().unwrap().into(),
        format: "hyper".into(),
        overwrite: true,
        ..ExportOptions::default()
    };
    for target in [&not_hyper, &a_dir] {
        let err = export_to_file(&te.engine, &hyper_export(target))
            .expect_err("only a Hyper file may be replaced");
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
        assert!(
            err.message.contains("not a Hyper database"),
            "{}",
            err.message
        );
    }
    assert_eq!(std::fs::read(&not_hyper).unwrap(), b"Hype? not a database");

    let real = dir.path().join("real.hyper");
    export_to_file(&te.engine, &hyper_export(&real)).unwrap();
    let first = std::fs::read(&real).unwrap();
    te.engine
        .execute_command("INSERT INTO test_export VALUES (3, 'Carol', 1.0)")
        .unwrap();
    let result = export_to_file(&te.engine, &hyper_export(&real)).unwrap();
    assert_eq!(result.rows, 3);
    assert_ne!(std::fs::read(&real).unwrap(), first);
    assert_eq!(
        dir_entries(dir.path()),
        ["dir.hyper", "real.hyper", "report.hyper"]
    );
}

/// A `hyper` export that fails after creating its database (an unknown
/// `source_db` fails once the target is attached) leaves the previous
/// file byte-identical and no temporary file beside it.
#[test]
fn failed_hyper_replace_leaves_the_previous_file() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("backup.hyper");
    let opts = |source_db: Option<&str>| ExportOptions {
        path: target.to_str().unwrap().into(),
        format: "hyper".into(),
        overwrite: true,
        source_db: source_db.map(Into::into),
        ..ExportOptions::default()
    };
    export_to_file(&te.engine, &opts(None)).unwrap();
    let before = std::fs::read(&target).unwrap();

    export_to_file(&te.engine, &opts(Some("no_such_db")))
        .expect_err("an unknown source database must fail");
    assert_eq!(std::fs::read(&target).unwrap(), before);
    assert_eq!(dir_entries(dir.path()), ["backup.hyper"]);
}

/// Every format refuses a protected file, even with `overwrite`, whichever
/// path reaches it: the file itself, a path through `.`, or a symlink.
#[test]
fn export_refuses_a_protected_file_by_identity() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let protected = dir.path().join("session.hyper");
    std::fs::write(&protected, b"Hyper\x08\0\0 session").unwrap();
    let targets = vec![
        protected.clone(),
        dir.path().join(".").join("session.hyper"),
    ];
    #[cfg(unix)]
    let targets = {
        let link = dir.path().join("link.hyper");
        std::os::unix::fs::symlink(&protected, &link).unwrap();
        targets.into_iter().chain([link]).collect::<Vec<_>>()
    };

    for target in &targets {
        for format in ["csv", "parquet", "arrow_ipc", "iceberg", "hyper"] {
            for overwrite in [true, false] {
                let opts = ExportOptions {
                    table: Some("test_export".into()),
                    path: target.to_str().unwrap().into(),
                    format: format.into(),
                    overwrite,
                    protected_paths: vec![protected.clone()],
                    ..ExportOptions::default()
                };
                let err = export_to_file(&te.engine, &opts)
                    .expect_err("a protected file must never be written");
                assert_eq!(
                    err.code,
                    ErrorCode::InvalidArgument,
                    "{format} overwrite={overwrite} {}: {err:?}",
                    target.display()
                );
            }
        }
    }
    assert_eq!(std::fs::read(&protected).unwrap(), b"Hyper\x08\0\0 session");
}

/// A directory holding a protected file is refused too: replacing an
/// Iceberg-shaped directory would delete the file inside it.
#[test]
fn export_refuses_a_directory_holding_a_protected_file() {
    let te = TestEngine::new_ephemeral();
    setup_test_table(&te);
    let dir = tempfile::tempdir().unwrap();
    let table = dir.path().join("table");
    std::fs::create_dir_all(table.join("metadata")).unwrap();
    std::fs::create_dir_all(table.join("data")).unwrap();
    std::fs::write(table.join("metadata").join("v1.metadata.json"), b"{}").unwrap();
    let protected = table.join("data").join("session.hyper");
    std::fs::write(&protected, b"Hyper\x08\0\0 session").unwrap();
    let before = tree_snapshot(&table);

    let opts = ExportOptions {
        protected_paths: vec![protected],
        ..sql_export("SELECT * FROM test_export", &table, "iceberg")
    };
    let err = export_to_file(&te.engine, &opts)
        .expect_err("a directory holding a protected file must not be replaced");
    assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
    assert_eq!(tree_snapshot(&table), before);
    assert_eq!(dir_entries(dir.path()), ["table"]);
}
