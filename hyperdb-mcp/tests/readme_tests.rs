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
const SMOKE_TESTS: &str = include_str!("../SMOKE_TESTS.md");
const DEMO: &str = include_str!("../examples/demo.rs");
const CHANGELOG: &str = include_str!("../CHANGELOG.md");
const DEVELOPMENT: &str = include_str!("../DEVELOPMENT.md");
const LIB_SOURCE: &str = include_str!("../src/lib.rs");
const SERVER_SOURCE: &str = include_str!("../src/server.rs");

fn markdown_section<'a>(text: &'a str, heading: &str, next_heading: &str) -> &'a str {
    let Some((_, after_heading)) = text.split_once(heading) else {
        return "";
    };
    let Some((section, _)) = after_heading.split_once(next_heading) else {
        return after_heading;
    };
    section
}

fn contains_any(text: &str, alternatives: &[&str]) -> bool {
    alternatives
        .iter()
        .any(|candidate| text.contains(candidate))
}

fn markdown_bullet(text: &str, needle: &str) -> String {
    let mut lines = Vec::new();
    let mut capturing = false;
    for line in text.lines() {
        if line.trim_start().starts_with("- ") {
            if capturing {
                break;
            }
            capturing = line.contains(needle);
        }
        if capturing {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn exact_smoke_response_lines<'a>(text: &'a str, tool: &str) -> Vec<&'a str> {
    text.lines()
        .filter_map(|line| {
            let (command, response) = line.split_once('→')?;
            (command.split_whitespace().next() == Some(tool)
                && response.contains('{')
                && !response.contains("..."))
            .then_some(line)
        })
        .collect()
}

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

/// The two user-facing READMEs must describe database routing and read-only
/// behavior using the same vocabulary and the tool handlers' actual guards.
/// This catches edits to the public/concise documentation, examples, and
/// static CLI reference, plus constructor rustdoc for removed workspace/bare
/// arguments, that drift from the generated tools or runtime.
#[test]
fn public_docs_database_and_read_only_contract() {
    const GUARDED_TOOLS: &[&str] = &[
        "execute",
        "load_data",
        "load_file",
        "load_files",
        "load_iceberg",
        "watch_directory",
        "save_query",
        "delete_query",
        "set_table_metadata",
        "copy_query",
        "kv_set",
        "kv_set_many",
        "kv_delete",
        "kv_pop",
        "kv_clear",
    ];

    let public = PUBLIC_README.to_lowercase();
    let concise = README.to_lowercase();
    let public_read_only = markdown_section(&public, "## read-only mode", "\n---");
    let public_allowed = markdown_section(public_read_only, "**allowed:**", "- **blocked:**");
    let public_blocked = markdown_section(public_read_only, "**blocked:**", "- **resources");
    let concise_rules = markdown_section(&concise, "## parameter rules", "## sql dialect");
    let public_chart = markdown_section(&public, "#### `chart`", "### incremental ingest");
    let concise_chart = markdown_section(
        &concise,
        "### chart delivery and presentation",
        "every successful database-routed",
    );
    let concise_examples = markdown_section(&concise, "## examples", "## tips for picking");
    let attach_example = markdown_section(
        concise_examples,
        "// cross-database join via attachment",
        "// read parquet",
    );
    let chart_example = markdown_section(concise_examples, "// chart", "\n```");
    let public_kv = markdown_section(&public, "### key-value scratchpad", "### export tools");
    let concise_kv = markdown_section(
        &concise,
        "### key-value store (scratchpad)",
        "**querying json",
    );
    let public_export = markdown_section(&public, "#### `export`", "### visualization");
    let concise_export = markdown_section(&concise, "### export", "### saved queries");
    let public_cli = markdown_section(&public, "## cli reference", "\n---");
    let concise_status = markdown_bullet(
        markdown_section(&concise, "### inspect", "### export"),
        "`status`",
    );
    let lib_source = LIB_SOURCE.to_lowercase();
    let engine_crate_doc = markdown_section(&lib_source, "- [`engine`]", "- [`ingest`]");
    let development = DEVELOPMENT.to_lowercase();
    let development_prerequisites =
        markdown_section(&development, "### prerequisites", "### build");
    let mut failures = Vec::new();

    if !concise_status.contains("daemon/hyper connection facts") {
        failures.push(
            "get_readme status guidance must describe daemon/Hyper connection facts".to_owned(),
        );
    }
    if concise_status.contains("daemon identity") {
        failures.push(
            "get_readme status guidance must not overclaim that status reports daemon identity"
                .to_owned(),
        );
    }

    if !(engine_crate_doc.contains("local database")
        && engine_crate_doc.contains("persistent database"))
    {
        failures.push(
            "crate-level engine documentation must use local and persistent database terminology"
                .to_owned(),
        );
    }
    if engine_crate_doc.contains("persistent workspace modes") {
        failures.push(
            "crate-level engine documentation must not claim persistent workspace modes".to_owned(),
        );
    }

    if !(development_prerequisites.contains("hyperd_path")
        && development_prerequisites.contains(".hyperd/current")
        && contains_any(
            development_prerequisites,
            &["walk upward", "search upward", "ancestor"],
        ))
    {
        failures.push(
            "DEVELOPMENT prerequisites must document HYPERD_PATH and upward .hyperd/current discovery"
                .to_owned(),
        );
    }
    if contains_any(
        development_prerequisites,
        &["place on `path`", "searches path", "path fallback"],
    ) {
        failures.push("DEVELOPMENT prerequisites must not claim PATH lookup".to_owned());
    }

    for (name, document) in [
        ("public README", public.as_str()),
        ("get_readme", concise.as_str()),
    ] {
        if !(document.contains("resource_busy")
            && document.contains("hyperdb-mcp doctor")
            && contains_any(
                document,
                &["possible owner", "holding process", "other process"],
            )
            && document.contains("copy"))
        {
            failures.push(format!(
                "{name} must explain contextual RESOURCE_BUSY recovery via doctor, the possible owner, and copying the file"
            ));
        }
        if !(document.contains("resolved_database")
            && contains_any(document, &["success", "successful"])
            && document.contains("local")
            && document.contains("persistent")
            && document.contains("attached"))
        {
            failures.push(format!(
                "{name} must explain resolved_database on routed successes using local/persistent/attached terminology"
            ));
        }

        let stale_workspace_lines: Vec<_> = document
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                if !line.contains("workspace")
                    || line.contains("--workspace")
                    || line.contains("workspace.hyper")
                    || line.contains("hyper://")
                    || line.contains("resource")
                    || line.contains(" uri")
                    || line.contains("histor")
                {
                    None
                } else {
                    Some(format!("{}: {}", index + 1, line.trim()))
                }
            })
            .collect();
        if !stale_workspace_lines.is_empty() {
            failures.push(format!(
                "{name} uses workspace as current database terminology outside compatibility/resource/history contexts:\n{}",
                stale_workspace_lines.join("\n")
            ));
        }
    }

    if !(attach_example.contains("attach_database({")
        && attach_example.contains("\"kind\": \"local_file\"")
        && attach_example.contains("lookup.public.dim_region"))
    {
        failures.push(
            "get_readme attach example must supply kind=local_file and use the runnable lookup.public.dim_region qualification"
                .to_owned(),
        );
    }
    if !(chart_example.contains("chart({")
        && chart_example.contains("\"chart_type\": \"bar\"")
        && chart_example.contains("\"x\":")
        && chart_example.contains("\"y\":"))
    {
        failures.push(
            "get_readme bar-chart example must supply the required x and y columns".to_owned(),
        );
    }

    for tool in GUARDED_TOOLS {
        if !public_blocked.contains(tool) {
            failures.push(format!(
                "public README blocked list is missing guarded tool {tool}"
            ));
        }
        if !concise_rules.contains(tool) {
            failures.push(format!(
                "get_readme read-only rules are missing guarded tool {tool}"
            ));
        }
    }
    for (name, section) in [
        ("public README", public_read_only),
        ("get_readme", concise_rules),
    ] {
        if !(section.contains("attach_database") && section.contains("writable")) {
            failures.push(format!(
                "{name} must say writable attach_database is guarded while read-only attachment remains available"
            ));
        }
    }
    if !(public_allowed.contains("unwatch_directory")
        && public_allowed.contains("export")
        && public_allowed.contains("hyper"))
    {
        failures.push(
            "public README must list unwatch_directory and Hyper-format export as allowed"
                .to_owned(),
        );
    }
    if public_blocked.contains("unwatch_directory") || public_blocked.contains("export") {
        failures.push(
            "public README must not list unwatch_directory or export among blocked tools"
                .to_owned(),
        );
    }
    if !(contains_any(
        concise_rules,
        &[
            "unwatch_directory remains allowed",
            "unwatch_directory stays allowed",
            "unwatch_directory is allowed",
        ],
    ) && concise_rules.contains("export")
        && concise_rules.contains("hyper")
        && contains_any(
            concise_rules,
            &["remain allowed", "stays allowed", "always work"],
        ))
    {
        failures.push(
            "get_readme must explicitly keep unwatch_directory and Hyper-format export allowed"
                .to_owned(),
        );
    }

    for (name, document) in [
        ("public README", public.as_str()),
        ("get_readme", concise.as_str()),
    ] {
        for token in [
            "quick diagnostic",
            "output_path",
            "inline",
            "png",
            "svg",
            "bar_orientation",
            "label_values",
            "show_legend",
            "y_scale",
            "proportional",
            "x_as_category",
        ] {
            if !document.contains(token) {
                failures.push(format!("{name} chart guidance is missing {token:?}"));
            }
        }
    }

    for parameter in ["database", "color_map", "label_points"] {
        if !public_chart.contains(&format!("`{parameter}`")) {
            failures.push(format!(
                "public README chart parameter table is missing `{parameter}`"
            ));
        }
        if !concise_chart.contains(parameter) {
            failures.push(format!(
                "get_readme chart guidance is missing `{parameter}`"
            ));
        }
    }
    for parameter in ["overwrite", "x_range", "y_range"] {
        if !concise_chart.contains(parameter) {
            failures.push(format!(
                "get_readme chart delivery/range guidance is missing `{parameter}`"
            ));
        }
    }

    for (name, kv_section) in [("public README", public_kv), ("get_readme", concise_kv)] {
        if !(kv_section.contains("attached")
            && kv_section.contains("writable")
            && contains_any(
                kv_section,
                &["even for readers", "including readers", "all kv_"],
            ))
        {
            failures.push(format!(
                "{name} must say all attached KV targets require writable access, including readers"
            ));
        }
    }

    for (name, export_section) in [
        (
            "public README",
            format!("{public_export}\n{public_read_only}"),
        ),
        ("get_readme", format!("{concise_export}\n{concise_rules}")),
    ] {
        if !(export_section.contains("source")
            && contains_any(
                &export_section,
                &[
                    "not mutate",
                    "does not mutate",
                    "leaves the source",
                    "source unchanged",
                ],
            )
            && export_section.contains("destination")
            && contains_any(
                &export_section,
                &["create", "replace", "materializ", "write"],
            ))
        {
            failures.push(format!(
                "{name} must explain that Hyper export leaves its source unchanged but creates/replaces a materialized destination"
            ));
        }
        for false_claim in ["read-only file copy", "only read database contents"] {
            if export_section.contains(false_claim) {
                failures.push(format!(
                    "{name} must not describe Hyper export as {false_claim:?}"
                ));
            }
        }
    }

    let daemon_command = public_cli
        .lines()
        .find(|line| line.trim_start().starts_with("daemon "))
        .unwrap_or("");
    if !daemon_command.contains("foreground") || daemon_command.contains("background") {
        failures.push(
            "public CLI command summary must describe `daemon` as foreground, not background"
                .to_owned(),
        );
    }
    if !(public_cli.contains("hyperdb_daemon_port")
        && public_cli.contains("auto-spawn")
        && public_cli.contains("configured/base")
        && public_cli.contains("exact")
        && contains_any(public_cli, &["pin", "candidate", "discovery"]))
    {
        failures.push(
            "public CLI reference must distinguish HYPERDB_DAEMON_PORT auto-spawn discovery from the foreground configured/base exact bind"
                .to_owned(),
        );
    }

    let constructor_doc = SERVER_SOURCE
        .split_once("    pub fn new(persistent_path: Option<String>, read_only: bool) -> Self")
        .and_then(|(before_signature, _)| {
            before_signature
                .rfind("    /// Create a server instance.")
                .map(|start| &before_signature[start..])
        })
        .unwrap_or("")
        .to_lowercase();
    if !(constructor_doc.contains("local database")
        && constructor_doc.contains("persistent database"))
    {
        failures.push(
            "HyperMcpServer::new rustdoc must describe the simultaneous local and optional persistent databases"
                .to_owned(),
        );
    }
    for stale_term in [
        "persistent workspace",
        "ephemeral workspace",
        "workspace mode",
        "`bare`",
        "`workspace_path`",
    ] {
        if constructor_doc.contains(stale_term) {
            failures.push(format!(
                "HyperMcpServer::new rustdoc still mentions stale term {stale_term:?}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "public database/read-only documentation failures:\n- {}",
        failures.join("\n- ")
    );
}

/// The executable smoke guide, demo commentary, and unreleased changelog must
/// describe the surfaces added or corrected in this release candidate.
/// This catches mutations to the smoke sequence/result examples and release
/// note claims that omit mandatory KV response fields or no longer match KV
/// routing or Hyper-export side effects.
#[test]
fn smoke_demo_and_changelog_contract() {
    let smoke = SMOKE_TESTS.to_lowercase();
    let demo = DEMO.to_lowercase();
    let unreleased =
        markdown_section(&CHANGELOG.to_lowercase(), "## [unreleased]", "\n## [").to_owned();
    let batch_section = markdown_section(
        &smoke,
        "## 2. create / read / overwrite (upsert)",
        "## 3. listing, size, store discovery",
    );
    let listing_section = markdown_section(
        &smoke,
        "## 3. listing, size, store discovery",
        "## 4. value fidelity",
    );
    let routing_section = markdown_section(
        &smoke,
        "## 8. database routing + isolation",
        "## 9. the `left join` enrichment pattern",
    );
    let batch_dense: String = batch_section
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let listing_dense: String = listing_section
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let mut failures = Vec::new();

    for token in ["kv_set_many", "resolved_database"] {
        if !smoke.contains(token) {
            failures.push(format!(
                "smoke guide is missing current tool/result token {token:?}"
            ));
        }
    }
    let batch_store = batch_section
        .lines()
        .find(|line| line.contains("kv_set_many"))
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|word| word.strip_prefix("store="))
        })
        .unwrap_or("");
    if batch_store.is_empty() || batch_store == "smoke" {
        failures.push(
            "kv_set_many smoke example must use a dedicated store so later smoke counts remain exact"
                .to_owned(),
        );
    } else {
        if !(batch_section.contains("kv_list")
            && batch_section.contains(&format!("store={batch_store}"))
            && batch_dense.contains("\"count\":2")
            && batch_dense.contains("\"keys\":[\"batch_a\",\"batch_b\"]"))
        {
            failures.push(
                "dedicated kv_set_many smoke store must be listed with count=2 and batch_a/b in lexicographic order"
                    .to_owned(),
            );
        }
        if !batch_section.contains(&format!("kv_clear store={batch_store}")) {
            failures.push(
                "dedicated kv_set_many smoke store must be cleared before later store-count checks"
                    .to_owned(),
            );
        }
    }
    if !(batch_dense.contains("\"stored\":2")
        && batch_dense.contains("\"created\":2")
        && batch_dense.contains("\"overwritten\":0")
        && listing_dense.contains("\"count\":4")
        && listing_dense.contains("[\"alpha\",\"bravo\",\"charlie\",\"greeting\"]"))
    {
        failures.push(
            "smoke batch/list examples must preserve exact counts and lexicographic key order"
                .to_owned(),
        );
    }
    let exact_kv_set_responses = exact_smoke_response_lines(&smoke, "kv_set");
    if exact_kv_set_responses.len() != 1 {
        failures.push(format!(
            "smoke guide must have one unmarked exact kv_set response, found {}",
            exact_kv_set_responses.len()
        ));
    }
    for response in exact_kv_set_responses {
        for field in ["\"created\"", "\"value_bytes\""] {
            if !response.contains(field) {
                failures.push(format!(
                    "unmarked exact kv_set response must include mandatory {field}: {}",
                    response.trim()
                ));
            }
        }
    }
    let exact_kv_size_responses = exact_smoke_response_lines(&smoke, "kv_size");
    if exact_kv_size_responses.len() < 5 {
        failures.push(format!(
            "smoke guide must retain at least five unmarked exact kv_size responses, found {}",
            exact_kv_size_responses.len()
        ));
    }
    for response in exact_kv_size_responses {
        if !response.contains("\"bytes\"") {
            failures.push(format!(
                "unmarked exact kv_size response must include mandatory bytes: {}",
                response.trim()
            ));
        }
    }
    for line in routing_section.lines().filter(|line| line.contains("→ {")) {
        if !line.contains("\"resolved_database\"") {
            failures.push(format!(
                "routed smoke expected JSON must include resolved_database or be labeled partial: {}",
                line.trim()
            ));
        }
    }
    if !(smoke.contains("hyperd_path")
        && smoke.contains(".hyperd/current")
        && contains_any(&smoke, &["walk upward", "search upward", "ancestor"]))
    {
        failures.push(
            "smoke guide must describe HYPERD_PATH executable/directory resolution and the upward .hyperd/current fallback"
                .to_owned(),
        );
    }
    if contains_any(
        &smoke,
        &[
            "or on `path`",
            "or on path",
            "searches path",
            "path fallback",
        ],
    ) {
        failures.push("smoke guide must not claim the runtime searches PATH".to_owned());
    }
    for token in [
        "hyperdb-mcp doctor",
        "side-effect-free",
        "engine_busy",
        "inconclusive",
        "resource_busy",
    ] {
        if !smoke.contains(token) {
            failures.push(format!("smoke guide is missing diagnostic token {token:?}"));
        }
    }

    if demo.contains("instead of failing") && demo.contains("numeric parse") {
        failures.push(
            "demo still claims DATE chart axes need categorical mode to avoid numeric parsing"
                .to_owned(),
        );
    }
    if !(demo.contains("date")
        && demo.contains("proportional")
        && contains_any(&demo, &["temporal axis", "time axis"]))
    {
        failures.push(
            "demo must describe the proportional temporal-axis behavior for its DATE chart"
                .to_owned(),
        );
    }

    for heading in ["### added", "### fixed", "### changed"] {
        let section = markdown_section(&unreleased, heading, "\n### ");
        if section.is_empty()
            || !section
                .lines()
                .any(|line| line.trim_start().starts_with("- "))
        {
            failures.push(format!(
                "crate ## [Unreleased] must contain at least one bullet under {heading}"
            ));
        }
    }
    for token in [
        "doctor",
        "resolved_database",
        "resource_busy",
        "health port",
        "engine_busy",
        "bar_orientation",
        "label_values",
        "show_legend",
        "y_scale",
    ] {
        if !unreleased.contains(token) {
            failures.push(format!(
                "crate ## [Unreleased] does not account for {token:?}"
            ));
        }
    }

    let hyper_export = markdown_bullet(&unreleased, "hyper-format export");
    if !(hyper_export.contains("source")
        && contains_any(
            &hyper_export,
            &[
                "not mutate",
                "does not mutate",
                "leaves the source",
                "source unchanged",
            ],
        )
        && hyper_export.contains("destination")
        && contains_any(&hyper_export, &["create", "replace", "materializ", "write"]))
    {
        failures.push(
            "crate changelog Hyper-export note must distinguish unchanged source from created/replaced materialized destination"
                .to_owned(),
        );
    }
    if contains_any(
        &hyper_export,
        &["read-only file copy", "harmless read-only file copy"],
    ) {
        failures.push(
            "crate changelog must not call Hyper export a harmless read-only file copy".to_owned(),
        );
    }

    assert!(
        failures.is_empty(),
        "smoke/demo/changelog documentation failures:\n- {}",
        failures.join("\n- ")
    );
}
