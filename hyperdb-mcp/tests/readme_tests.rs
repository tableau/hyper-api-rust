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

/// The concise, LLM-facing README must be sufficient to choose the three chart
/// presentation controls without guessing at defaults or label sizing.
#[test]
fn readme_chart_presentation_contract() {
    fn surrounding_lines(text: &str, needle: &str, radius: usize) -> String {
        let lines: Vec<_> = text.lines().collect();
        let Some(index) = lines.iter().position(|line| line.contains(needle)) else {
            return String::new();
        };
        let start = index.saturating_sub(radius);
        let end = (index + radius + 1).min(lines.len());
        lines[start..end].join("\n").to_lowercase()
    }

    let orientation = surrounding_lines(README, "bar_orientation", 4);
    let values = surrounding_lines(README, "label_values", 4);
    let legend = surrounding_lines(README, "show_legend", 4);
    let chart = surrounding_lines(README, "`chart`", 20);
    let mut failures: Vec<String> = Vec::new();

    if !(orientation.contains("bar_orientation")
        && orientation.contains("vertical")
        && orientation.contains("horizontal"))
    {
        failures.push(
            "README must name bar_orientation and both vertical/horizontal choices".to_string(),
        );
    }
    if !(values.contains("label_values")
        && values.contains("value")
        && ["original", "exact", "verbatim"]
            .iter()
            .any(|word| values.contains(word)))
    {
        failures.push("README must say label_values uses the original/exact scalar".to_string());
    }
    if !(legend.contains("show_legend")
        && legend.contains("default")
        && legend.contains("true")
        && ["false", "suppress", "hide"]
            .iter()
            .any(|word| legend.contains(word)))
    {
        failures
            .push("README must document show_legend=true by default and suppression".to_string());
    }
    for required in [
        "long", "unicode", "truncat", "auto", "siz", "clip", "width", "height",
    ] {
        if !chart.contains(required) {
            failures.push(format!(
                "README chart layout caveat is missing semantic token {required:?}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "chart presentation README failures:\n{}",
        failures.join("\n")
    );
}

/// The LLM-facing README must make the positive-only logarithmic contract
/// actionable without inviting unsupported x-log/symlog/histogram guesses.
#[test]
fn readme_chart_log_contract() {
    fn surrounding_lines(text: &str, needle: &str, radius: usize) -> String {
        let lines: Vec<_> = text.lines().collect();
        let Some(index) = lines.iter().position(|line| line.contains(needle)) else {
            return String::new();
        };
        let start = index.saturating_sub(radius);
        let end = (index + radius + 1).min(lines.len());
        lines[start..end].join("\n").to_lowercase()
    }

    let scale = surrounding_lines(README, "y_scale", 8);
    let chart = surrounding_lines(README, "`chart`", 28);
    let mut failures: Vec<String> = Vec::new();

    if !(scale.contains("y_scale")
        && scale.contains("linear")
        && scale.contains("log")
        && scale.contains("default"))
    {
        failures.push("README must document y_scale with linear default and log choice".into());
    }
    if !(scale.contains("positive")
        && (scale.contains("zero") || scale.contains("> 0"))
        && scale.contains("negative"))
    {
        failures.push("README must state that log values/ranges are strictly positive".into());
    }
    if !(scale.contains("horizontal") && scale.contains('y') && scale.contains("measure")) {
        failures.push(
            "README must tie y_scale to the data-role y measure even for horizontal bars".into(),
        );
    }
    if !scale.contains("histogram") {
        failures.push("README must say logarithmic histograms are unsupported".into());
    }
    if !(scale.contains("range")
        && ["contain", "enclos", "include"]
            .iter()
            .any(|word| scale.contains(word))
        && scale.contains("value"))
    {
        failures.push("README must require explicit log ranges to contain every value".into());
    }
    if !(chart.contains("bar")
        && chart.contains("lower")
        && chart.contains("bound")
        && ["never zero", "not zero", "instead of zero"]
            .iter()
            .any(|phrase| chart.contains(phrase)))
    {
        failures
            .push("README must say log bars start at the positive lower bound, not zero".into());
    }

    assert!(
        failures.is_empty(),
        "chart log README failures:\n{}",
        failures.join("\n")
    );
}
