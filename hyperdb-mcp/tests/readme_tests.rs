// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Structural checks on the `get_readme` payload defined in
//! [`hyperdb_mcp::readme`].
//!
//! These tests don't lock in exact prose — they pin the README to the
//! actual tool surface and to the few invariants we always want
//! present. If a tool is added or renamed, `readme_mentions_every_tool`
//! fails until [`hyperdb-mcp/src/readme.rs`] is updated to match.

use hyperdb_mcp::readme::README;

const PUBLIC_README: &str = include_str!("../README.md");

#[test]
fn readme_is_non_trivial() {
    assert!(
        README.len() > 500,
        "README looks empty/stub: {} bytes",
        README.len()
    );
    assert!(
        README.len() < 20_000,
        "README is too long for a tool response: {} bytes",
        README.len()
    );
}

#[test]
fn readme_mentions_every_tool() {
    // If you add or rename a tool, update this list AND the README.
    // The two must stay in sync.
    let tools = [
        "query",
        "query_data",
        "query_file",
        "execute",
        "load_file",
        "load_files",
        "load_data",
        "load_iceberg",
        "describe",
        "sample",
        "inspect_file",
        "status",
        "export",
        "chart",
        "copy_query",
        "save_query",
        "delete_query",
        "set_table_metadata",
        "attach_database",
        "detach_database",
        "list_attached_databases",
        "watch_directory",
        "unwatch_directory",
        "kv_get",
        "kv_set",
        "kv_set_many",
        "kv_delete",
        "kv_list",
        "kv_list_stores",
        "kv_size",
        "kv_pop",
        "kv_clear",
        "get_readme",
    ];
    for tool in tools {
        assert!(README.contains(tool), "README missing mention of `{tool}`");
    }
}

#[test]
fn readme_includes_sql_dialect_pointers() {
    assert!(
        README.contains("PostgreSQL"),
        "README should call out PostgreSQL compatibility"
    );
    assert!(
        README.contains("read-only"),
        "README should mention read-only mode constraints"
    );
    assert!(
        README.contains("Hyper"),
        "README should identify the underlying Hyper engine"
    );
}

#[test]
fn doctor_readme_contract() {
    fn command_on_line(line: &str) -> &str {
        let line = line.trim().strip_prefix("$ ").unwrap_or(line.trim());
        line.split_once("  #")
            .map_or(line, |(command, _comment)| command.trim_end())
    }

    fn has_exact_command(readme: &str, command: &str) -> bool {
        readme.lines().any(|line| command_on_line(line) == command)
    }

    assert!(
        has_exact_command(PUBLIC_README, "hyperdb-mcp doctor"),
        "public README must show the exact human-report command `hyperdb-mcp doctor`"
    );
    assert!(
        has_exact_command(PUBLIC_README, "hyperdb-mcp doctor --json"),
        "public README must show the exact machine-report command `hyperdb-mcp doctor --json`"
    );

    let lines: Vec<_> = PUBLIC_README.lines().collect();
    let command_line = lines
        .iter()
        .position(|line| command_on_line(line) == "hyperdb-mcp doctor --json")
        .expect("exact doctor --json command was asserted above");
    let start = command_line.saturating_sub(12);
    let end = (command_line + 21).min(lines.len());
    let doctor_scope = lines[start..end].join("\n").to_lowercase();

    assert!(
        doctor_scope.contains("side-effect-free") || doctor_scope.contains("side effect free"),
        "doctor documentation must state that collection is side-effect-free:\n{doctor_scope}"
    );
    assert!(
        [
            "does not start",
            "doesn't start",
            "without starting",
            "never starts",
            "will not start",
        ]
        .iter()
        .any(|phrase| doctor_scope.contains(phrase))
            && (doctor_scope.contains("daemon") || doctor_scope.contains("hyperd"))
            && doctor_scope.contains("database"),
        "doctor documentation must say it starts neither a daemon nor a database:\n{doctor_scope}"
    );
    assert!(
        doctor_scope.contains("local paths")
            && doctor_scope.contains("review")
            && doctor_scope.contains("shar"),
        "doctor documentation must warn users to review local paths before sharing:\n{doctor_scope}"
    );
}

/// `status` can return a deliberately partial response while another tool
/// owns the engine mutex. The LLM-facing README must prevent clients from
/// treating that fallback as definitive and tell them how to obtain full data.
#[test]
fn readme_degraded_status_contract() {
    assert!(
        README.contains("engine_busy: true"),
        "README must name the degraded-status signal"
    );
    assert!(
        README.contains("partial"),
        "README must label engine_busy status as partial/non-definitive"
    );
    assert!(
        README.contains("hyperd_running: false"),
        "README must document the degraded hyperd_running value"
    );
    assert!(
        README.contains("inconclusive") || README.contains("non-definitive"),
        "README must say degraded hyperd_running: false is inconclusive"
    );
    assert!(
        README.contains("retry"),
        "README must guide clients to retry status for full statistics"
    );
}
