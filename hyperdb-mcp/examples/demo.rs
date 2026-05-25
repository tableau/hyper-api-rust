// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Live demo of the `hyperdb-mcp` engine.
//!
//! Simulates two weeks of a Rust developer's sprint — daily coffee
//! consumption, lines of code, bugs found / fixed, commits, self-reported
//! mood, and a happiness score — then walks through the engine's public
//! API end-to-end: ingest → describe → sample → aggregate → rank →
//! chart.
//!
//! # Running
//!
//! From the workspace root with `HYPERD_PATH` pointing at a `hyperd`
//! binary:
//!
//! ```bash
//! HYPERD_PATH=/path/to/hyperd cargo run --example demo -p hyperdb-mcp
//! ```
//!
//! # Outputs
//!
//! All artifacts land in `hyperdb-mcp/demo_output/`:
//!
//! - `coder_stats.csv` — the synthetic input dataset, inspectable with
//!   any CSV viewer.
//! - `happiness_over_time.png` — line chart of daily happiness score.
//! - `coffee_vs_bugs.png` — scatter of coffee consumption against bug
//!   count.
//!
//! Between sections the demo prints the SQL it ran and a compact text
//! rendering of each result set, so the console output doubles as a
//! reproducible recipe the reader can paste into their own MCP client.

use hyperdb_mcp::chart::{render_chart, ChartFormat, ChartOptions, ChartResult, ChartType};
use hyperdb_mcp::engine::Engine;
use hyperdb_mcp::ingest::{ingest_csv_file, IngestOptions};
use serde_json::Value;
use std::path::{Path, PathBuf};

// ─────────────────────────────── Fake data ───────────────────────────────

/// Two weeks of a fictional developer's sprint. Patterns embedded so the
/// later queries and charts tell a coherent story:
/// - Day 5 is a deploy day → bugs spike even though morale is still ok.
/// - Days 6–7 are a weekend → low output, high happiness.
/// - Day 12 is an all-nighter → coffee spikes, happiness tanks.
/// - Day 14 is ship day → rapid bug-fix, commits peak, happiness recovers.
const CSV_HEADER: &str =
    "day,coffee_cups,lines_written,bugs_found,bugs_fixed,commits,mood,happiness_score";

const CSV_ROWS: &[&str] = &[
    "2026-04-06,3,420,2,3,5,happy,8",
    "2026-04-07,2,310,1,2,3,ok,6",
    "2026-04-08,4,580,3,3,8,happy,7",
    "2026-04-09,3,640,2,4,9,happy,8",
    "2026-04-10,5,720,5,4,6,grumpy,5",
    "2026-04-11,1,50,0,1,1,ok,6",
    "2026-04-12,0,0,0,0,0,euphoric,10",
    "2026-04-13,3,380,1,2,4,happy,7",
    "2026-04-14,4,450,2,3,5,ok,6",
    "2026-04-15,3,500,1,2,5,happy,8",
    "2026-04-16,5,600,3,3,7,happy,8",
    "2026-04-17,8,850,2,1,3,grumpy,4",
    "2026-04-18,2,200,4,3,2,grumpy,5",
    "2026-04-19,4,550,1,5,10,euphoric,9",
];

// ─────────────────────────── Console prettiness ──────────────────────────

/// Print a title banner with unicode horizontal lines. Lives inline rather
/// than pulling in a dependency — this is a demo, not a framework.
fn section(title: &str) {
    println!();
    println!(
        "━━━ {title} {}",
        "━".repeat(80usize.saturating_sub(title.len() + 5))
    );
}

/// Subsection header — same banner idea, one level quieter.
fn step(title: &str) {
    println!();
    println!("▸ {title}");
}

/// Render a list of JSON object rows (as returned by
/// [`Engine::execute_query_to_json`] or [`Engine::sample_table`]) into a
/// plain-text aligned table. Keeps the demo dependency-free while still
/// looking like a database console.
fn print_rows_as_table(rows: &[Value]) {
    if rows.is_empty() {
        println!("   (no rows)");
        return;
    }

    // Collect headers from the first row, preserving insertion order.
    let headers: Vec<String> = if let Some(obj) = rows[0].as_object() {
        obj.keys().cloned().collect()
    } else {
        for r in rows {
            println!("   {r}");
        }
        return;
    };

    // Format every cell as a trimmed string. Nested arrays / objects
    // (e.g. the `columns` field on `describe_tables`) are truncated to
    // something that fits in a single column rather than exploding the
    // table width.
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            headers
                .iter()
                .map(|h| match r.get(h) {
                    None | Some(Value::Null) => "NULL".to_string(),
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Number(n)) => n.to_string(),
                    Some(Value::Bool(b)) => b.to_string(),
                    Some(Value::Array(a)) => format!("<{} items>", a.len()),
                    Some(Value::Object(o)) => format!("<object, {} keys>", o.len()),
                })
                .collect()
        })
        .collect();

    // Compute column widths: max of header and data lengths, min 3.
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let data_max = cells.iter().map(|row| row[i].len()).max().unwrap_or(0);
            h.len().max(data_max).max(3)
        })
        .collect();

    let sep: String = widths
        .iter()
        .map(|w| "─".repeat(*w))
        .collect::<Vec<_>>()
        .join("─┼─");

    // Header line.
    let header_line: String = headers
        .iter()
        .zip(widths.iter())
        .map(|(h, w)| format!("{h:<w$}"))
        .collect::<Vec<_>>()
        .join(" │ ");
    println!("   {header_line}");
    println!("   {sep}");

    // Data lines.
    for row in &cells {
        let line: String = row
            .iter()
            .zip(widths.iter())
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join(" │ ");
        println!("   {line}");
    }
}

/// Print a SQL block — indented with a pipe so it stands out from the
/// prose above it.
fn sql(query: &str) {
    println!();
    for line in query.lines() {
        println!("   │ {line}");
    }
    println!();
}

// ─────────────────────────── Engine operations ───────────────────────────

/// Write the embedded dataset to `demo_output/coder_stats.csv` so the
/// `COPY FROM` in `ingest_csv_file` has an on-disk source.
fn write_csv(output_dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(output_dir)?;
    let path = output_dir.join("coder_stats.csv");
    let mut body = String::with_capacity(1024);
    body.push_str(CSV_HEADER);
    body.push('\n');
    for row in CSV_ROWS {
        body.push_str(row);
        body.push('\n');
    }
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Save a rendered chart to disk and return its path.
fn write_chart(output_dir: &Path, name: &str, chart: &ChartResult) -> std::io::Result<PathBuf> {
    let ext = if chart.mime_type.contains("svg") {
        "svg"
    } else {
        "png"
    };
    let path = output_dir.join(format!("{name}.{ext}"));
    std::fs::write(&path, &chart.bytes)?;
    Ok(path)
}

// ───────────────────────────────── Main ──────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Mute hyperd's info-level churn; the demo produces its own output.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let output_dir = PathBuf::from("hyperdb-mcp/demo_output");

    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║  hyperdb-mcp · live demo                                                    ║");
    println!("║  Two weeks in the life of a Rust developer (synthesized, obviously)          ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");

    // ── Step 0: synthesize CSV ──────────────────────────────────────────
    section("Step 0 · Stage the data");
    let csv_path = write_csv(&output_dir)?;
    println!("   Wrote {} rows of sprint data to:", CSV_ROWS.len());
    println!("   {}", csv_path.display());

    // ── Step 1: spin up engine ─────────────────────────────────────────
    section("Step 1 · Launch the engine (ephemeral workspace)");
    let engine = Engine::new(None)?;
    println!("   Ephemeral DB: {}", engine.ephemeral_path().display());
    println!("   Log dir:   {}", engine.log_dir().display());
    println!("   hyperd is running: {}", engine.is_running());

    // ── Step 2: ingest CSV ─────────────────────────────────────────────
    section("Step 2 · Ingest — schema is inferred, not declared");
    let ingest_opts = IngestOptions {
        table: "coder_stats".into(),
        mode: "replace".into(),
        schema_override: None,
        merge_key: None,
        target_db: None,
    };
    let ingest_result = ingest_csv_file(&engine, csv_path.to_str().unwrap(), &ingest_opts)?;
    println!(
        "   Ingested {} rows into `coder_stats`.",
        ingest_result.rows
    );
    println!(
        "   Schema inference took {} ms",
        ingest_result.stats.schema_inference_ms.unwrap_or(0)
    );
    println!();
    println!("   Inferred columns:");
    for col in &ingest_result.schema {
        let nullable = if col.nullable { "nullable" } else { "not null" };
        println!(
            "     · {:<18} {:<15} ({nullable})",
            col.name, col.hyper_type
        );
    }

    // ── Step 3: describe ───────────────────────────────────────────────
    section("Step 3 · Describe — what's in the workspace?");
    let tables = engine.describe_tables()?;
    print_rows_as_table(&tables);

    // ── Step 4: sample ────────────────────────────────────────────────
    section("Step 4 · Sample — the first five rows");
    let sample = engine.sample_table("coder_stats", 5)?;
    // sample_table returns a JSON object with metadata; its "rows" field
    // is the data array we want to render.
    let sample_rows: &[Value] = sample
        .get("rows")
        .and_then(|v| v.as_array())
        .map_or(&[], std::vec::Vec::as_slice);
    print_rows_as_table(sample_rows);

    // ── Step 5: aggregates by mood ────────────────────────────────────
    section("Step 5 · Does mood predict output?");
    // `ROUND(AVG(...), n)` is purely cosmetic — it trims the default
    // 6 decimal places Hyper uses for `AVG(INTEGER)` results down to
    // something readable in the demo table. No CAST needed: AVG
    // returns `NUMERIC(16, 6)`, and the mcp engine's JSON renderer
    // decodes NUMERIC via `row.get::<Numeric>()` directly.
    let q5 = "SELECT mood,
                     COUNT(*) AS days,
                     ROUND(AVG(coffee_cups), 1) AS avg_coffee,
                     ROUND(AVG(lines_written), 0) AS avg_lines,
                     ROUND(AVG(bugs_found), 1) AS avg_bugs_found,
                     ROUND(AVG(happiness_score), 1) AS avg_happiness
              FROM coder_stats
              GROUP BY mood
              ORDER BY avg_happiness DESC";
    sql(q5);
    let result = engine.execute_query_to_json(q5)?;
    print_rows_as_table(&result);

    // ── Step 6: coffee tiers ───────────────────────────────────────────
    section("Step 6 · Coffee-load tiers — too much of a good thing?");
    let q6 = "SELECT CASE
                       WHEN coffee_cups = 0 THEN '☕ none'
                       WHEN coffee_cups <= 2 THEN '☕ light (1-2)'
                       WHEN coffee_cups <= 4 THEN '☕ regular (3-4)'
                       ELSE '☕ danger zone (5+)'
                     END AS coffee_tier,
                     COUNT(*) AS days,
                     ROUND(AVG(bugs_found), 1) AS avg_bugs_found,
                     ROUND(AVG(happiness_score), 1) AS avg_happiness,
                     MAX(lines_written) AS best_output
              FROM coder_stats
              GROUP BY coffee_tier
              ORDER BY avg_happiness DESC";
    sql(q6);
    let result = engine.execute_query_to_json(q6)?;
    print_rows_as_table(&result);

    // ── Step 7: top 3 / bottom 3 days by happiness ────────────────────
    section("Step 7 · Rankings");
    step("Top 3 days");
    let q7a = "SELECT day, coffee_cups, lines_written, bugs_fixed, mood, happiness_score
               FROM coder_stats
               ORDER BY happiness_score DESC, commits DESC
               LIMIT 3";
    sql(q7a);
    let result = engine.execute_query_to_json(q7a)?;
    print_rows_as_table(&result);

    step("Bottom 3 days");
    let q7b = "SELECT day, coffee_cups, lines_written, bugs_found, mood, happiness_score
               FROM coder_stats
               ORDER BY happiness_score ASC, bugs_found DESC
               LIMIT 3";
    sql(q7b);
    let result = engine.execute_query_to_json(q7b)?;
    print_rows_as_table(&result);

    // ── Step 8: render charts ──────────────────────────────────────────
    section("Step 8 · Render charts");

    // Line chart: happiness over time.
    step("Line chart · happiness_score vs day");
    let chart_query = "SELECT day, happiness_score
                       FROM coder_stats
                       ORDER BY day";
    sql(chart_query);
    let chart_rows = engine.execute_query_to_json(chart_query)?;
    let line_chart = render_chart(
        &chart_rows,
        &ChartOptions {
            chart_type: ChartType::Line,
            x_column: Some("day".into()),
            y_column: Some("happiness_score".into()),
            title: Some("Daily happiness over two weeks".into()),
            format: ChartFormat::Png,
            width: 900,
            height: 400,
            // `day` is a DATE column — plot it as categorical so the
            // axis ticks render as ISO strings instead of failing
            // numeric parse.
            x_as_category: Some(true),
            ..ChartOptions::default()
        },
    )?;
    let line_path = write_chart(&output_dir, "happiness_over_time", &line_chart)?;
    println!(
        "   {} · {} rows plotted · {} bytes",
        line_path.display(),
        line_chart.rows_plotted,
        line_chart.bytes.len()
    );

    // Scatter: coffee vs bugs.
    step("Scatter chart · bugs_found vs coffee_cups");
    let chart_query2 = "SELECT coffee_cups, bugs_found, mood
                        FROM coder_stats
                        ORDER BY coffee_cups";
    sql(chart_query2);
    let chart_rows2 = engine.execute_query_to_json(chart_query2)?;
    let scatter_chart = render_chart(
        &chart_rows2,
        &ChartOptions {
            chart_type: ChartType::Scatter,
            x_column: Some("coffee_cups".into()),
            y_column: Some("bugs_found".into()),
            series_column: Some("mood".into()),
            title: Some("Coffee consumption vs bugs discovered".into()),
            format: ChartFormat::Png,
            width: 900,
            height: 400,
            ..ChartOptions::default()
        },
    )?;
    let scatter_path = write_chart(&output_dir, "coffee_vs_bugs", &scatter_chart)?;
    println!(
        "   {} · {} rows plotted · {} bytes",
        scatter_path.display(),
        scatter_chart.rows_plotted,
        scatter_chart.bytes.len()
    );

    // ── Step 9: engine status ──────────────────────────────────────────
    section("Step 9 · Engine status");
    let status = engine.status()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&status).unwrap_or_default()
    );

    // ── Outro ─────────────────────────────────────────────────────────
    section("Demo complete");
    println!("   Artifacts:");
    println!("   · {}", csv_path.display());
    println!("   · {}", line_path.display());
    println!("   · {}", scatter_path.display());
    println!();
    println!("   The engine's temp workspace will be removed when this");
    println!("   process exits (workspace is ephemeral, `is_persistent = false`).");
    println!();

    Ok(())
}
