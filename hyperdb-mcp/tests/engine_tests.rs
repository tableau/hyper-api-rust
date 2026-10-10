// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the core Engine: process lifecycle, DDL/DML execution, catalog
//! introspection, and workspace status reporting.

mod common;

use common::TestEngine;
use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::error::ErrorCode;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const PERSISTENT_LOCK_CHILD_ENV: &str = "HYPERDB_MCP_PERSISTENT_LOCK_CHILD";

/// Verify that creating an Engine successfully starts hyperd and establishes
/// a live connection.
#[test]
fn engine_starts_and_connects() {
    let te = TestEngine::new_ephemeral();
    assert!(te.engine.is_running());
}

/// A second private engine must diagnose a locked reserved persistent
/// attachment without risking a permanently blocked integration-test worker.
/// The parent owns the exact helper child and always kills + waits on timeout.
#[test]
fn real_persistent_lock_reproduces_resource_busy() {
    if let Some(workspace) = std::env::var_os(PERSISTENT_LOCK_CHILD_ENV) {
        run_real_persistent_lock_child(Path::new(&workspace));
        return;
    }

    let temp_dir = TempDir::new().expect("parent must own RAII workspace directory");
    let workspace = temp_dir.path().join("contended-persistent.hyper");
    run_contained_child(
        "real_persistent_lock_reproduces_resource_busy",
        PERSISTENT_LOCK_CHILD_ENV,
        &workspace,
    );
}

fn run_real_persistent_lock_child(workspace: &Path) {
    let effective_path = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let owner = Engine::new_no_daemon(Some(workspace.to_string_lossy().into_owned()))
        .expect("owner engine must attach the persistent workspace first");
    let error = Engine::new_no_daemon(Some(workspace.to_string_lossy().into_owned()))
        .expect_err("second private engine must reject the contended persistent workspace");

    assert_eq!(error.code, ErrorCode::ResourceBusy);
    assert!(
        error.message.contains("55006"),
        "must retain the lock SQLSTATE: {}",
        error.message
    );
    assert!(
        error.message.to_lowercase().contains("attached")
            || error.message.to_lowercase().contains("lock"),
        "must retain Hyper's raw lock diagnostic: {}",
        error.message
    );
    assert!(
        error.message.contains(effective_path.to_str().unwrap()),
        "must report the exact effective persistent path {}: {}",
        effective_path.display(),
        error.message
    );
    let guidance = error
        .suggestion
        .expect("persistent lock needs recovery guidance");
    let guidance_lower = guidance.to_lowercase();
    assert!(
        guidance_lower.contains("doctor"),
        "guidance must point to doctor: {guidance}"
    );
    assert!(
        guidance_lower.contains("possible")
            && (guidance_lower.contains("owner") || guidance_lower.contains("process")),
        "guidance must describe a possible owner without accusation: {guidance}"
    );

    drop(owner);
}

/// Regression test for #277: `Engine::execute_attach_command` is the
/// mapper `export.rs`'s `CREATE DATABASE`/`ATTACH DATABASE` pair now
/// shares with `attach.rs`'s ATTACH statement. It must classify a REAL
/// lock conflict from a live `hyperd` as `RESOURCE_BUSY` with the
/// SQLSTATE preserved, not fall through to a generic `SqlError` the way
/// the plain `execute_command` path does since the `0.7.3` narrowing of
/// the generic mapper (`error.rs`'s `sqlstate().is_none()` guard).
///
/// Reproduces the conflict against the live, pinned `hyperd` binary: an
/// owner engine holds `workspace` open as its own persistent database
/// (its own separate `hyperd` process), and a second, independent
/// engine issues a raw `ATTACH DATABASE` against that same path through
/// `execute_attach_command` — the exact statement shape `export.rs` and
/// `attach.rs` both issue. Run in a contained child for the same timeout
/// safety reasons as `real_persistent_lock_reproduces_resource_busy`.
#[test]
fn execute_attach_command_maps_real_lock_conflict_to_resource_busy() {
    if let Some(workspace) = std::env::var_os(EXPORT_ATTACH_LOCK_CHILD_ENV) {
        run_execute_attach_command_lock_child(Path::new(&workspace));
        return;
    }

    let temp_dir = TempDir::new().expect("parent must own RAII workspace directory");
    let workspace = temp_dir.path().join("contended-attach-target.hyper");
    run_contained_child(
        "execute_attach_command_maps_real_lock_conflict_to_resource_busy",
        EXPORT_ATTACH_LOCK_CHILD_ENV,
        &workspace,
    );
}

const EXPORT_ATTACH_LOCK_CHILD_ENV: &str = "HYPERDB_MCP_EXPORT_ATTACH_LOCK_CHILD";

fn run_execute_attach_command_lock_child(target: &Path) {
    // Owner: a separate hyperd process holds `target` as its own
    // persistent workspace for the whole duration of this check.
    let owner = Engine::new_no_daemon(Some(target.to_string_lossy().into_owned()))
        .expect("owner engine must attach the target as its own persistent workspace");
    assert!(target.exists(), "owner must have created the file");

    // A second, independent engine (its own hyperd process) is the one
    // issuing the contended statement — standing in for either
    // `export.rs`'s export-target ATTACH or `attach.rs`'s user attach.
    let temp_dir = TempDir::new().expect("child must own its own temp dir");
    let exporter_path = temp_dir.path().join("exporter_primary.hyper");
    let exporter = Engine::new_no_daemon(Some(exporter_path.to_string_lossy().into_owned()))
        .expect("exporter engine must start on its own workspace");

    let sql = format!(
        "ATTACH DATABASE {} AS \"probe\"",
        hyperdb_api::escape_sql_path(&target.to_string_lossy())
    );
    let err = exporter
        .execute_attach_command(&sql, target)
        .expect_err("attach against an externally-locked file must fail");

    assert_eq!(
        err.code,
        ErrorCode::ResourceBusy,
        "expected RESOURCE_BUSY, got {err:?}"
    );
    assert!(
        err.message.contains("55006"),
        "must retain the lock SQLSTATE: {}",
        err.message
    );
    let guidance = err
        .suggestion
        .expect("RESOURCE_BUSY must carry recovery guidance");
    assert!(
        guidance.to_lowercase().contains("doctor"),
        "guidance must point to doctor: {guidance}"
    );

    drop(owner);
}

fn run_contained_child(test_name: &str, child_env: &str, workspace: &Path) {
    let mut child =
        Command::new(std::env::current_exe().expect("integration test executable path"))
            .args(["--exact", test_name, "--nocapture"])
            .env(child_env, workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("parent must spawn the exact lock helper child");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .expect("parent must collect completed helper output");
                assert!(
                    status.success(),
                    "lock helper failed with {status}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
                return;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let kill_error = child.kill().err();
                let output = child
                    .wait_with_output()
                    .expect("parent must wait for timed-out helper child");
                panic!(
                    "lock helper exceeded its 15s bound and was killed ({kill_error:?})\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
            Err(error) => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .expect("parent must wait after helper status error");
                panic!(
                    "lock helper status check failed: {error}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
        }
    }
}

/// Tables whose names begin with `_hyperdb_` are `HyperDB` infrastructure
/// (saved-queries meta-table today, future watcher/audit state
/// tomorrow) and must be hidden from both `describe_tables` and the
/// `status` `table_count` / `total_rows` totals so user-facing views
/// never surface internal bookkeeping.
#[test]
fn engine_hides_hyperdb_internal_tables() {
    let te = TestEngine::new_ephemeral();
    // Create one user table and one meta-table using the documented
    // `_hyperdb_` prefix — the saved-queries store uses the exact same
    // convention.
    te.engine
        .execute_command("CREATE TABLE widgets (id INT, label TEXT)")
        .unwrap();
    te.engine
        .execute_command(
            "CREATE TABLE _hyperdb_saved_queries (\
                 name TEXT, sql TEXT, description TEXT, created_at TIMESTAMP)",
        )
        .unwrap();
    te.engine
        .execute_command("INSERT INTO widgets VALUES (1, 'alpha'), (2, 'beta')")
        .unwrap();

    let tables = te.engine.describe_tables().unwrap();
    assert_eq!(tables.len(), 1, "meta-table should be hidden from describe");
    assert_eq!(tables[0]["name"], "widgets");

    let status = te.engine.status().unwrap();
    assert_eq!(
        status["table_count"], 1,
        "table_count should exclude meta-tables: {status}"
    );
    assert_eq!(
        status["total_rows"], 2,
        "total_rows should not count meta-table rows: {status}"
    );
}

/// Round-trip test: create a table, insert a row via SQL, then query it back
/// as JSON and verify column values are correctly deserialized.
#[test]
fn engine_create_table_and_query() {
    let te = TestEngine::new_ephemeral();
    te.engine
        .execute_command("CREATE TABLE test (id INT, name TEXT)")
        .unwrap();
    te.engine
        .execute_command("INSERT INTO test VALUES (1, 'Alice')")
        .unwrap();
    let rows = te
        .engine
        .execute_query_to_json("SELECT * FROM test")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], 1);
    assert_eq!(rows[0]["name"], "Alice");
}

/// Verify that `describe_tables` returns catalog metadata including table name,
/// column definitions, and row counts for tables in the public schema.
#[test]
fn engine_describe_tables() {
    let te = TestEngine::new_ephemeral();
    te.engine
        .execute_command("CREATE TABLE orders (id INT NOT NULL, amount DOUBLE PRECISION)")
        .unwrap();
    let tables = te.engine.describe_tables().unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0]["name"], "orders");
}

/// Verify that `describe_table(name)` returns the same JSON shape as an entry
/// of `describe_tables`, restricted to the requested table. Covers the
/// single-table path used by the `describe` MCP tool when `table` is set.
#[test]
fn engine_describe_single_table() {
    let te = TestEngine::new_ephemeral();
    te.engine
        .execute_command("CREATE TABLE widgets (id INT NOT NULL, label TEXT)")
        .unwrap();
    te.engine
        .execute_command("CREATE TABLE gadgets (sku TEXT, price DOUBLE PRECISION)")
        .unwrap();
    te.engine
        .execute_command("INSERT INTO widgets VALUES (1, 'alpha'), (2, 'beta')")
        .unwrap();

    let widgets = te.engine.describe_table("widgets").unwrap();
    assert_eq!(widgets["name"], "widgets");
    assert_eq!(widgets["row_count"], 2);
    let cols = widgets["columns"].as_array().expect("columns array");
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0]["name"], "id");
    assert_eq!(cols[0]["nullable"], false);
    assert_eq!(cols[1]["name"], "label");
    assert_eq!(cols[1]["nullable"], true);

    // Missing table must surface as TABLE_NOT_FOUND, not a raw Hyper error.
    let err = te.engine.describe_table("does_not_exist").unwrap_err();
    assert_eq!(err.code, ErrorCode::TableNotFound);
}

/// Internal `_hyperdb_*` bookkeeping tables are hidden from `describe_tables`,
/// and probing them directly via `describe_table(name)` must also surface as
/// `TABLE_NOT_FOUND` so infrastructure isn't addressable through the public
/// describe path either.
#[test]
fn engine_describe_table_hides_internal() {
    let te = TestEngine::new_ephemeral();
    te.engine
        .execute_command(
            "CREATE TABLE _hyperdb_saved_queries (\
                 name TEXT, sql TEXT, description TEXT, created_at TIMESTAMP)",
        )
        .unwrap();
    let err = te
        .engine
        .describe_table("_hyperdb_saved_queries")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::TableNotFound);
}

/// Verify that `status()` reports hyperd health and workspace metrics for an
/// empty workspace (zero tables, zero rows).
#[test]
fn engine_status() {
    let te = TestEngine::new_ephemeral();
    let status = te.engine.status().unwrap();
    assert_eq!(status["hyperd_running"], true);
    assert_eq!(status["table_count"], 0);
    // Version field: a single top-level `hyper_rust_api_version` string
    // of the form `<semver>.r<hash>` (optionally `-dirty`). We don't
    // lock a specific hash, just assert the field is populated so a
    // regression that drops it from the status payload surfaces.
    let version = status["hyper_rust_api_version"]
        .as_str()
        .expect("hyper_rust_api_version string");
    assert!(
        version.contains(".r"),
        "hyper_rust_api_version should carry .r<hash>: {version}"
    );

    // Engine connection block. This is a `--no-daemon` engine (TestEngine uses
    // `Engine::new_no_daemon`), so mode is "local", there's a private hyperd
    // endpoint, and no daemon health endpoint.
    let engine_info = &status["engine"];
    assert_eq!(engine_info["mode"], "local");
    assert!(
        engine_info["hyperd_endpoint"].is_string(),
        "local engine should report a private hyperd endpoint, got {:?}",
        engine_info["hyperd_endpoint"]
    );
    assert!(
        engine_info["daemon_health_endpoint"].is_null(),
        "local engine has no daemon health endpoint"
    );

    // Decomposed connection forms (issue #124). The transport is whatever
    // `HyperProcess` actually negotiated, so assert consistency rather than
    // hardcoding TCP: host/port are populated only for TCP, socket_path only
    // for the IPC transports, and the descriptor always re-attaches the
    // scheme `hyperd` used.
    let conn = &engine_info["connection"];
    let endpoint = engine_info["hyperd_endpoint"].as_str().unwrap();
    assert_eq!(
        conn["endpoint"], endpoint,
        "connection.endpoint must mirror hyperd_endpoint"
    );
    let descriptor = conn["connection_descriptor"]
        .as_str()
        .expect("a live engine's endpoint must classify into a descriptor");
    match conn["transport"].as_str().unwrap() {
        "tcp" => {
            assert_eq!(descriptor, format!("tab.tcp://{endpoint}"));
            assert!(conn["host"].is_string(), "TCP endpoint must report a host");
            assert!(conn["port"].is_u64(), "TCP endpoint must report a port");
            assert!(conn["socket_path"].is_null(), "TCP has no socket path");
        }
        "unix_domain_socket" | "named_pipe" => {
            assert!(
                conn["host"].is_null() && conn["port"].is_null(),
                "an IPC endpoint has no host:port — reporting one would be fabricated"
            );
            assert_eq!(conn["socket_path"], endpoint);
        }
        other => panic!("unexpected transport {other} for endpoint {endpoint}"),
    }
}

/// Regression test: calling `create_table` twice in append mode must be
/// idempotent. Before using CREATE TABLE IF NOT EXISTS internally, a racy
/// `has_table` probe could return a false negative and cause the second call
/// to fail with "42P07 table already exists", which then aborted the
/// connection. This verifies the new code handles duplicate calls cleanly.
#[test]
fn create_table_append_is_idempotent() {
    use hyperdb_mcp::schema::ColumnSchema;
    let te = TestEngine::new_ephemeral();
    let cols = vec![
        ColumnSchema {
            name: "id".into(),
            hyper_type: "INT".into(),
            nullable: false,
        },
        ColumnSchema {
            name: "name".into(),
            hyper_type: "TEXT".into(),
            nullable: true,
        },
    ];

    te.engine.create_table("idempotent", &cols, false).unwrap();
    // Second call in append mode must succeed without error.
    te.engine.create_table("idempotent", &cols, false).unwrap();
    // Third call with a COMPLETELY different schema is also no-op in append
    // mode: CREATE TABLE IF NOT EXISTS is silent when the table exists, it
    // does not verify schema compatibility.
    let other = vec![ColumnSchema {
        name: "different".into(),
        hyper_type: "BIGINT".into(),
        nullable: true,
    }];
    te.engine.create_table("idempotent", &other, false).unwrap();

    // Confirm the original schema survived — the "different" column was not
    // added.
    let tables = te.engine.describe_tables().unwrap();
    let t = tables.iter().find(|t| t["name"] == "idempotent").unwrap();
    let cols = t["columns"].as_array().unwrap();
    let names: Vec<&str> = cols.iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(names.contains(&"id"));
    assert!(names.contains(&"name"));
    assert!(!names.contains(&"different"));
}

/// Regression test: `create_table` in replace mode always overwrites, even
/// when the target table already exists.
#[test]
fn create_table_replace_overwrites() {
    use hyperdb_mcp::schema::ColumnSchema;
    let te = TestEngine::new_ephemeral();
    let cols_a = vec![ColumnSchema {
        name: "a".into(),
        hyper_type: "INT".into(),
        nullable: true,
    }];
    let cols_b = vec![ColumnSchema {
        name: "b".into(),
        hyper_type: "TEXT".into(),
        nullable: true,
    }];

    te.engine.create_table("t", &cols_a, true).unwrap();
    te.engine.create_table("t", &cols_b, true).unwrap();

    let tables = te.engine.describe_tables().unwrap();
    let t = tables.iter().find(|t| t["name"] == "t").unwrap();
    let names: Vec<&str> = t["columns"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(names, vec!["b"]);
}

/// Regression test: `create_table` rejects an unknown type name BEFORE issuing
/// any DDL, so a typo in a schema override can't corrupt connection state.
#[test]
fn create_table_rejects_unknown_type() {
    use hyperdb_mcp::error::ErrorCode;
    use hyperdb_mcp::schema::ColumnSchema;
    let te = TestEngine::new_ephemeral();
    let cols = vec![ColumnSchema {
        name: "x".into(),
        hyper_type: "NOT_A_REAL_TYPE".into(),
        nullable: true,
    }];
    let err = te.engine.create_table("t", &cols, false).unwrap_err();
    assert_eq!(err.code, ErrorCode::SchemaMismatch);

    // Connection should still be usable after the rejection.
    te.engine.execute_command("CREATE TABLE u (v INT)").unwrap();
}

/// Log paths: engine exposes `log_dir()` and `status()` includes a `logs`
/// object with the directory path. The actual `hyperd.log` file shows up
/// shortly after startup.
#[test]
fn engine_exposes_log_paths() {
    let te = TestEngine::new_ephemeral();
    let log_dir = te.engine.log_dir();
    assert!(log_dir.exists(), "log_dir should have been created");

    let status = te.engine.status().unwrap();
    let logs = status.get("logs").expect("status should include logs");
    assert!(logs["log_dir"].is_string());
    // hyperd_log may or may not exist yet depending on timing — tolerate both.
    // The field must at least be present (null or string).
    assert!(logs.get("hyperd_log").is_some());
    assert!(logs.get("client_log").is_some());
}

/// Regression test: NUMERIC columns and NUMERIC-typed aggregate results
/// (`AVG`, `SUM`, etc.) both render as JSON numbers through
/// `execute_query_to_json`.
///
/// Before the upstream NUMERIC precision/scale fix (and the matching mcp
/// adoption of `row.get::<Numeric>()`), two cases silently dropped to
/// `null` in the JSON output:
///
///   1. `NUMERIC(p, s)` column values — scale was lost during
///      `RowDescription` parsing, so the 16-byte wire bytes got decoded
///      with `scale = 0`, producing e.g. `1250` instead of `12.50`.
///   2. `AVG(INT)` results — Hyper returns these as the 8-byte
///      `Numeric(16, 6)` wire form, which the old mcp path (16-byte-only
///      `Numeric::decode`) couldn't handle at all, so they came back as
///      `null`.
///
/// Both now flow through `row.get::<Numeric>()`, which is schema-aware
/// (reads scale from the column's `SqlType::Numeric`) and dispatches on
/// buffer length, so this test exercises both cases end-to-end.
#[test]
fn engine_numeric_columns_and_aggregates_render_as_json_numbers() {
    let te = TestEngine::new_ephemeral();
    te.engine
        .execute_command("CREATE TABLE sales (id INT, price NUMERIC(10, 2))")
        .unwrap();
    te.engine
        .execute_command("INSERT INTO sales VALUES (1, 12.50), (2, 7.25), (3, 100.00)")
        .unwrap();

    // Case 1: NUMERIC column scale is preserved — 12.50 must not come
    // back as 1250 or null.
    let rows = te
        .engine
        .execute_query_to_json("SELECT id, price FROM sales ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 3);
    let prices: Vec<f64> = rows
        .iter()
        .map(|r| {
            r["price"]
                .as_f64()
                .unwrap_or_else(|| panic!("price rendered as non-number: {:?}", r["price"]))
        })
        .collect();
    assert!((prices[0] - 12.50).abs() < 1e-9, "got {}", prices[0]);
    assert!((prices[1] - 7.25).abs() < 1e-9, "got {}", prices[1]);
    assert!((prices[2] - 100.00).abs() < 1e-9, "got {}", prices[2]);

    // Case 2: AVG returns the 8-byte Numeric wire form (precision ≤ 18).
    // The old path returned null; we now decode it correctly.
    let avg_rows = te
        .engine
        .execute_query_to_json("SELECT AVG(id) AS avg_id FROM sales")
        .unwrap();
    assert_eq!(avg_rows.len(), 1);
    let avg = avg_rows[0]["avg_id"]
        .as_f64()
        .unwrap_or_else(|| panic!("AVG rendered as non-number: {:?}", avg_rows[0]["avg_id"]));
    assert!((avg - 2.0).abs() < 1e-9, "got {avg}");
}

/// Regression test: negative NUMERIC values with magnitude < 1 (the open
/// interval `(-1, 0)`) must keep their sign through `execute_query_to_json`.
///
/// `row_value_to_json` serializes NUMERIC via `Numeric::to_string()`, whose
/// `Display` impl previously derived the sign from the integer part alone.
/// For `-0.5` the integer part is `0` (which prints without a sign), so the
/// value rendered as `0.5` — silently flipping the sign of correlations,
/// 0–1 indices, and regression residuals. Values with `|x| >= 1` and the
/// `DOUBLE PRECISION` path were unaffected, which this test also guards.
#[test]
fn engine_negative_sub_unit_numeric_keeps_sign() {
    let te = TestEngine::new_ephemeral();

    let rows = te
        .engine
        .execute_query_to_json(
            "SELECT \
                 CAST(-0.5   AS numeric(10,4)) AS a, \
                 CAST(-0.999 AS numeric(10,4)) AS b, \
                 CAST(-1.5   AS numeric(10,4)) AS c, \
                 CAST(-0.5   AS double precision) AS d",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];

    let val = |k: &str| {
        row[k]
            .as_f64()
            .unwrap_or_else(|| panic!("{k} rendered as non-number: {:?}", row[k]))
    };
    assert!((val("a") - (-0.5)).abs() < 1e-9, "got {}", val("a"));
    assert!((val("b") - (-0.999)).abs() < 1e-9, "got {}", val("b"));
    assert!((val("c") - (-1.5)).abs() < 1e-9, "got {}", val("c"));
    assert!((val("d") - (-0.5)).abs() < 1e-9, "got {}", val("d"));
}

/// NUMERIC values that survive an `f64` round-trip keep the compact JSON
/// number shape that consumers already depend on.
///
/// The guard here is against over-correcting the precision fix below: a
/// *textual* round-trip check would flag `9.50` as lossy (it stringifies as
/// `"9.50"`, becomes `9.5` as an `f64`, and formats back as `"9.5"`), pushing
/// ordinary two-decimal money values into JSON strings. `2^53` is the sharp
/// case — 16 significant digits, but exactly representable, so a rule that
/// merely counted digits would stringify it needlessly.
#[test]
fn engine_f64_exact_numerics_stay_json_numbers() {
    let te = TestEngine::new_ephemeral();

    let rows = te
        .engine
        .execute_query_to_json(
            "SELECT \
                 CAST('9.50' AS NUMERIC(8,2))              AS money, \
                 CAST('0.1' AS NUMERIC(8,1))               AS tenth, \
                 CAST('0' AS NUMERIC(10,2))                AS zero, \
                 CAST('-2.25' AS NUMERIC(10,2))            AS negative, \
                 CAST('999999999999999' AS NUMERIC(15,0))  AS fifteen_digits, \
                 CAST('9007199254740992' AS NUMERIC(16,0)) AS two_pow_53",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];

    let num = |k: &str| {
        row[k]
            .as_f64()
            .unwrap_or_else(|| panic!("{k} must stay a JSON number, got {:?}", row[k]))
    };
    assert!((num("money") - 9.5).abs() < 1e-9, "got {}", num("money"));
    assert!((num("tenth") - 0.1).abs() < 1e-9, "got {}", num("tenth"));
    assert!((num("zero")).abs() < 1e-9, "got {}", num("zero"));
    assert!(
        (num("negative") - (-2.25)).abs() < 1e-9,
        "got {}",
        num("negative")
    );
    assert!((num("fifteen_digits") - 999_999_999_999_999_f64).abs() < 1.0);
    assert!((num("two_pow_53") - 9_007_199_254_740_992_f64).abs() < 1.0);
}

/// NUMERIC values that an `f64` cannot represent must arrive as the exact
/// decimal text rather than a silently rounded JSON number.
///
/// `CAST('99999999999999999.99' AS NUMERIC(19,2))` was emitted as `1e17`:
/// `Numeric::to_string()` is exact, but the `parse::<f64>()` that followed
/// discarded the low digits, and the string fallback never fired because
/// `1e17` is a perfectly finite `f64`. `2^53 + 1` is the adjacent-value
/// proof that the rule tracks representability rather than digit count —
/// it differs from `two_pow_53` above by one, and only it is a string.
#[test]
fn engine_f64_lossy_numerics_render_as_exact_strings() {
    let te = TestEngine::new_ephemeral();

    let rows = te
        .engine
        .execute_query_to_json(
            "SELECT \
                 CAST('99999999999999999.99' AS NUMERIC(19,2))  AS big, \
                 CAST('-99999999999999999.99' AS NUMERIC(19,2)) AS big_negative, \
                 CAST('9007199254740993' AS NUMERIC(16,0))      AS two_pow_53_plus_1",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];

    assert_eq!(row["big"], serde_json::json!("99999999999999999.99"));
    assert_eq!(
        row["big_negative"],
        serde_json::json!("-99999999999999999.99")
    );
    assert_eq!(
        row["two_pow_53_plus_1"],
        serde_json::json!("9007199254740993")
    );
}

/// `resolve_log_dir` helper: persistent mode uses the workspace's parent,
/// ephemeral mode uses the per-PID temp dir.
#[test]
fn resolve_log_dir_picks_expected_paths() {
    use hyperdb_mcp::engine::resolve_log_dir;
    let persistent = resolve_log_dir(Some("/tmp/subdir/project.hyper"));
    assert_eq!(persistent, std::path::PathBuf::from("/tmp/subdir"));

    let ephemeral = resolve_log_dir(None);
    let expected_prefix = format!("hyperdb-mcp-{}", std::process::id());
    assert!(
        ephemeral
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s == expected_prefix),
        "ephemeral log dir should end with {expected_prefix}, got {ephemeral:?}"
    );
}
