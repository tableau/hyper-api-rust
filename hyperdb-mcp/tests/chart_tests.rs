// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for the chart module: PNG/SVG output, each chart type, and error
//! paths for bad input. These tests don't need a Hyper engine — they invoke
//! `render_chart` directly with synthetic row data.

use hyperdb_mcp::chart::{
    auto_generated_chart_path, parse_hex_color, render_chart, resolve_chart_disposition,
    resolve_chart_format, write_chart_to_disk, ChartDisposition, ChartFormat, ChartOptions,
    ChartType,
};
use hyperdb_mcp::error::ErrorCode;
use serde_json::json;

fn bar_opts() -> ChartOptions {
    ChartOptions {
        chart_type: ChartType::Bar,
        x_column: Some("product".into()),
        y_column: Some("revenue".into()),
        ..ChartOptions::default()
    }
}

/// PNG output should start with the 8-byte PNG magic signature.
#[test]
fn bar_chart_png_has_magic_bytes() {
    let rows = vec![
        json!({"product": "Widget", "revenue": 100}),
        json!({"product": "Gadget", "revenue": 250}),
        json!({"product": "Gizmo", "revenue": 80}),
    ];
    let opts = bar_opts();
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.mime_type, "image/png");
    assert!(result.rows_plotted >= 3);
    // PNG magic bytes: 89 50 4E 47 0D 0A 1A 0A
    assert!(result
        .bytes
        .starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]));
}

/// SVG output should begin with an SVG tag.
#[test]
fn bar_chart_svg_starts_with_svg_tag() {
    let rows = vec![
        json!({"product": "A", "revenue": 10}),
        json!({"product": "B", "revenue": 20}),
    ];
    let opts = ChartOptions {
        format: ChartFormat::Svg,
        ..bar_opts()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.mime_type, "image/svg+xml");
    let text = String::from_utf8_lossy(&result.bytes);
    assert!(
        text.contains("<svg"),
        "SVG output should contain <svg tag: {}",
        &text[..text.len().min(200)]
    );
}

/// Line chart renders with numeric x and y columns and multiple series.
#[test]
fn line_chart_multi_series() {
    let rows = vec![
        json!({"t": 1, "v": 10.0, "g": "A"}),
        json!({"t": 2, "v": 15.0, "g": "A"}),
        json!({"t": 3, "v": 12.0, "g": "A"}),
        json!({"t": 1, "v": 5.0, "g": "B"}),
        json!({"t": 2, "v": 8.0, "g": "B"}),
        json!({"t": 3, "v": 11.0, "g": "B"}),
    ];
    let opts = ChartOptions {
        chart_type: ChartType::Line,
        x_column: Some("t".into()),
        y_column: Some("v".into()),
        series_column: Some("g".into()),
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 6);
    assert_eq!(result.mime_type, "image/png");
}

/// Line chart can plot over a string (categorical) x axis when the
/// caller opts in via `x_as_category: Some(true)`. Exercises the path
/// the demo uses for plotting a `DATE` column — without the opt-in the
/// chart would fail with "column is missing or not numeric".
#[test]
fn line_chart_categorical_x() {
    let rows = vec![
        json!({"day": "Mon", "score": 7}),
        json!({"day": "Tue", "score": 8}),
        json!({"day": "Wed", "score": 6}),
        json!({"day": "Thu", "score": 9}),
        json!({"day": "Fri", "score": 10}),
    ];
    let opts = ChartOptions {
        chart_type: ChartType::Line,
        x_column: Some("day".into()),
        y_column: Some("score".into()),
        x_as_category: Some(true),
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 5);
    assert_eq!(result.mime_type, "image/png");
    // Without explicit x_as_category, auto-detection kicks in and
    // recognizes the string x column as categorical.
    let without_category = ChartOptions {
        x_as_category: None,
        ..opts
    };
    let result2 = render_chart(&rows, &without_category).unwrap();
    assert_eq!(result2.rows_plotted, 5);
}

/// Scatter chart with no series column renders every row as one series.
#[test]
fn scatter_chart_single_series() {
    let rows: Vec<_> = (0..20)
        .map(|i| json!({"x": f64::from(i), "y": f64::from(i).sin()}))
        .collect();
    let opts = ChartOptions {
        chart_type: ChartType::Scatter,
        x_column: Some("x".into()),
        y_column: Some("y".into()),
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 20);
}

/// Histogram takes a single numeric column and bins it.
#[test]
fn histogram_chart_counts_values() {
    let rows: Vec<_> = (0..100).map(|i| json!({"v": f64::from(i % 10)})).collect();
    let opts = ChartOptions {
        chart_type: ChartType::Histogram,
        x_column: Some("v".into()),
        bins: 10,
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 100);
}

/// Missing required x column returns a schema-mismatch error with a helpful message.
#[test]
fn missing_x_column_errors() {
    let rows = vec![json!({"y": 1.0})];
    let opts = ChartOptions {
        chart_type: ChartType::Bar,
        x_column: None,
        y_column: Some("y".into()),
        ..ChartOptions::default()
    };
    let err = render_chart(&rows, &opts).unwrap_err();
    assert_eq!(err.code, ErrorCode::SchemaMismatch);
    assert!(err.message.contains("'x'"));
}

/// Empty rows vector is rejected with `EmptyData`.
#[test]
fn empty_rows_rejected() {
    let rows = vec![];
    let opts = bar_opts();
    let err = render_chart(&rows, &opts).unwrap_err();
    assert_eq!(err.code, ErrorCode::EmptyData);
}

/// Non-numeric y values on a bar chart surface a `SchemaMismatch` error.
#[test]
fn non_numeric_y_errors() {
    let rows = vec![json!({"x": "A", "y": "not a number"})];
    let opts = bar_opts();
    let err = render_chart(&rows, &opts).unwrap_err();
    assert_eq!(err.code, ErrorCode::SchemaMismatch);
}

/// `ChartType::parse` accepts valid names and rejects unknown ones.
#[test]
fn chart_type_parse() {
    assert_eq!(ChartType::parse("bar").unwrap(), ChartType::Bar);
    assert_eq!(ChartType::parse("LINE").unwrap(), ChartType::Line);
    assert_eq!(ChartType::parse("Scatter").unwrap(), ChartType::Scatter);
    assert_eq!(ChartType::parse("histogram").unwrap(), ChartType::Histogram);
    assert_eq!(ChartType::parse("hist").unwrap(), ChartType::Histogram);
    assert!(ChartType::parse("pie").is_err());
}

/// `ChartFormat::parse` accepts png/svg (case-insensitive) and rejects others.
#[test]
fn chart_format_parse() {
    assert_eq!(ChartFormat::parse("png").unwrap(), ChartFormat::Png);
    assert_eq!(ChartFormat::parse("SVG").unwrap(), ChartFormat::Svg);
    assert!(ChartFormat::parse("jpeg").is_err());
}

/// `parse_hex_color` accepts #rrggbb and rrggbb (without prefix), rejects bad input.
#[test]
fn hex_color_parse() {
    let c = parse_hex_color("#e41a1c").unwrap();
    assert_eq!((c.0, c.1, c.2), (0xe4, 0x1a, 0x1c));
    let c = parse_hex_color("ff7f0e").unwrap(); // no leading #
    assert_eq!((c.0, c.1, c.2), (0xff, 0x7f, 0x0e));
    assert!(parse_hex_color("not-a-color").is_none());
    assert!(parse_hex_color("#gg0000").is_none()); // invalid hex digit
    assert!(parse_hex_color("#fff").is_none()); // too short
}

/// `x_range` and `y_range` fix the axis extents; the chart still renders with
/// data that falls within the specified range.
#[test]
fn scatter_fixed_axes_render() {
    let rows: Vec<_> = (0..10)
        .map(|i| json!({"x": f64::from(i) * 100.0, "y": f64::from(i) * 0.1}))
        .collect();
    let opts = ChartOptions {
        chart_type: ChartType::Scatter,
        x_column: Some("x".into()),
        y_column: Some("y".into()),
        x_range: Some([0.0, 1500.0]),
        y_range: Some([0.0, 1.0]),
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 10);
    assert_eq!(result.mime_type, "image/png");
}

/// `x_range` and `y_range` also work on line charts.
#[test]
fn line_fixed_axes_render() {
    let rows = vec![
        json!({"t": 1924.0, "v": 0.15}),
        json!({"t": 1974.0, "v": 0.58}),
        json!({"t": 2023.0, "v": 0.37}),
    ];
    let opts = ChartOptions {
        chart_type: ChartType::Line,
        x_column: Some("t".into()),
        y_column: Some("v".into()),
        x_range: Some([1900.0, 2030.0]),
        y_range: Some([0.0, 1.0]),
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 3);
    assert_eq!(result.mime_type, "image/png");
}

/// `label_points=true` renders without a legend box and still produces a valid
/// PNG. The chart should not error even when every series has one point.
#[test]
fn scatter_label_points_renders() {
    let rows = vec![
        json!({"x": 1438.0, "y": 0.37, "country": "India"}),
        json!({"x": 1422.0, "y": 0.07, "country": "China"}),
        json!({"x": 343.0,  "y": 0.85, "country": "United States"}),
    ];
    let opts = ChartOptions {
        chart_type: ChartType::Scatter,
        x_column: Some("x".into()),
        y_column: Some("y".into()),
        series_column: Some("country".into()),
        x_range: Some([0.0, 1500.0]),
        y_range: Some([0.0, 1.0]),
        label_points: true,
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 3);
    assert_eq!(result.mime_type, "image/png");
    // PNG magic bytes confirm a valid image was produced.
    assert!(result.bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]));
}

/// `label_points=false` (default) still renders legend — regression guard.
#[test]
fn scatter_legend_by_default() {
    let rows = vec![
        json!({"x": 1.0, "y": 0.5, "s": "A"}),
        json!({"x": 2.0, "y": 0.8, "s": "B"}),
    ];
    let opts = ChartOptions {
        chart_type: ChartType::Scatter,
        x_column: Some("x".into()),
        y_column: Some("y".into()),
        series_column: Some("s".into()),
        ..ChartOptions::default() // label_points defaults to false
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 2);
}

/// `color_map` assigns the specified colors to named series; unmapped series
/// still render (they fall back to the default palette).
#[test]
fn scatter_color_map_renders() {
    let rows = vec![
        json!({"x": 1438.0, "y": 0.37, "country": "India"}),
        json!({"x": 1422.0, "y": 0.07, "country": "China"}),
        json!({"x": 343.0,  "y": 0.85, "country": "United States"}),
        json!({"x": 211.0,  "y": 0.79, "country": "Brazil"}),
    ];
    let color_map = [
        ("India".to_string(), parse_hex_color("#e41a1c").unwrap()),
        ("China".to_string(), parse_hex_color("#ff7f0e").unwrap()),
    ]
    .into_iter()
    .collect();
    let opts = ChartOptions {
        chart_type: ChartType::Scatter,
        x_column: Some("x".into()),
        y_column: Some("y".into()),
        series_column: Some("country".into()),
        x_range: Some([0.0, 1500.0]),
        y_range: Some([0.0, 1.0]),
        color_map,
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    // All 4 rows rendered; India+China use supplied colors, US+Brazil fall
    // back to palette — the important thing is it doesn't panic or error.
    assert_eq!(result.rows_plotted, 4);
    assert_eq!(result.mime_type, "image/png");
}

// ─── Format / path / disposition resolution ────────────────────────────

/// With neither `format` nor `output_path`, the default is PNG.
#[test]
fn resolve_format_defaults_to_png() {
    assert_eq!(resolve_chart_format(None, None).unwrap(), ChartFormat::Png);
}

/// Explicit `format="svg"` without a path is honored.
#[test]
fn resolve_format_explicit_only() {
    assert_eq!(
        resolve_chart_format(Some("svg"), None).unwrap(),
        ChartFormat::Svg
    );
    assert_eq!(
        resolve_chart_format(Some("PNG"), None).unwrap(),
        ChartFormat::Png
    );
}

/// Path extension drives the format when `format` is omitted.
#[test]
fn resolve_format_from_path_extension() {
    assert_eq!(
        resolve_chart_format(None, Some("/tmp/chart.png")).unwrap(),
        ChartFormat::Png
    );
    assert_eq!(
        resolve_chart_format(None, Some("/tmp/chart.SVG")).unwrap(),
        ChartFormat::Svg
    );
    // Path with no extension: fall back to PNG default.
    assert_eq!(
        resolve_chart_format(None, Some("/tmp/chart")).unwrap(),
        ChartFormat::Png
    );
}

/// Agreement between `format` and path extension passes through.
#[test]
fn resolve_format_agreement_ok() {
    assert_eq!(
        resolve_chart_format(Some("png"), Some("/tmp/x.png")).unwrap(),
        ChartFormat::Png
    );
    assert_eq!(
        resolve_chart_format(Some("svg"), Some("/tmp/x.svg")).unwrap(),
        ChartFormat::Svg
    );
}

/// Mismatch between `format` and path extension is a loud error the
/// caller can fix by dropping one side.
#[test]
fn resolve_format_mismatch_errors() {
    let err = resolve_chart_format(Some("svg"), Some("/tmp/x.png")).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("svg"));
    assert!(err.message.contains(".png"));

    let err = resolve_chart_format(Some("png"), Some("/tmp/x.svg")).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

/// Unknown extensions (.jpg, .bmp) are rejected with a clear list of
/// supported options.
#[test]
fn resolve_format_unknown_extension_errors() {
    let err = resolve_chart_format(None, Some("/tmp/x.jpg")).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("jpg"));
    assert!(err.message.contains("png") && err.message.contains("svg"));
}

/// `inline=true` with no path → inline-only, no write.
#[test]
fn disposition_inline_only() {
    let d = resolve_chart_disposition(true, None, ChartFormat::Png);
    assert_eq!(d, ChartDisposition::InlineOnly);
    assert!(d.wants_inline());
    assert!(d.path().is_none());
}

/// `inline=true` + `output_path` → both.
#[test]
fn disposition_inline_and_write() {
    let d = resolve_chart_disposition(true, Some("/tmp/x.png"), ChartFormat::Png);
    assert!(matches!(d, ChartDisposition::WriteAndInline { .. }));
    assert!(d.wants_inline());
    assert_eq!(d.path().unwrap(), std::path::Path::new("/tmp/x.png"));
}

/// `inline=false` + `output_path` → write only.
#[test]
fn disposition_write_only_explicit_path() {
    let d = resolve_chart_disposition(false, Some("/tmp/x.svg"), ChartFormat::Svg);
    assert!(matches!(d, ChartDisposition::WriteOnly { .. }));
    assert!(!d.wants_inline());
    assert_eq!(d.path().unwrap(), std::path::Path::new("/tmp/x.svg"));
}

/// The default case — `inline=false` + no path — writes to an
/// auto-generated path under `std::env::temp_dir()/hyperdb-charts/`.
#[test]
fn disposition_write_only_auto_path() {
    let d = resolve_chart_disposition(false, None, ChartFormat::Png);
    assert!(matches!(d, ChartDisposition::WriteOnly { .. }));
    assert!(!d.wants_inline());
    let path = d.path().unwrap();
    assert!(
        path.starts_with(std::env::temp_dir().join("hyperdb-charts")),
        "auto path should live under temp_dir()/hyperdb-charts, got: {}",
        path.display()
    );
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
}

/// SVG auto-paths end in `.svg`.
#[test]
fn auto_path_svg_extension() {
    let path = auto_generated_chart_path(ChartFormat::Svg);
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("svg"));
}

/// Two consecutive auto-path calls produce distinct paths — the counter
/// makes this deterministic even when the clock ticks slower than the
/// test loop.
#[test]
fn auto_path_is_unique() {
    let a = auto_generated_chart_path(ChartFormat::Png);
    let b = auto_generated_chart_path(ChartFormat::Png);
    assert_ne!(a, b);
}

// ─── Disk write behavior ────────────────────────────────────────────────

/// Writing to a fresh path creates the parent directory and the file.
#[test]
fn write_creates_parent_dir_and_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub1").join("sub2").join("chart.png");
    let bytes = b"\x89PNG\r\n\x1a\nfake";

    let n = write_chart_to_disk(&path, bytes, true).unwrap();
    assert_eq!(n, bytes.len() as u64);
    assert!(path.exists(), "file should have been written");
    let on_disk = std::fs::read(&path).unwrap();
    assert_eq!(on_disk, bytes);
}

/// `overwrite=true` (default) silently replaces an existing file.
#[test]
fn write_overwrite_true_replaces_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chart.png");
    std::fs::write(&path, b"old").unwrap();

    write_chart_to_disk(&path, b"new", true).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
}

/// `overwrite=false` refuses to touch an existing file and returns
/// `PermissionDenied`. The original bytes remain on disk.
#[test]
fn write_overwrite_false_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chart.png");
    std::fs::write(&path, b"keep me").unwrap();

    let err = write_chart_to_disk(&path, b"replacement", false).unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    assert!(err.message.contains("overwrite=true"));
    // Original untouched.
    assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
}

/// `overwrite=false` on a non-existing path is fine — nothing to clobber.
#[test]
fn write_overwrite_false_new_path_ok() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new-chart.png");
    assert!(!path.exists());

    write_chart_to_disk(&path, b"content", false).unwrap();
    assert!(path.exists());
}

/// End-to-end: render a real PNG and write it to a temp path. The file
/// on disk must start with the PNG magic bytes.
#[test]
fn render_and_write_produces_valid_png_on_disk() {
    let rows = vec![
        json!({"product": "A", "revenue": 10}),
        json!({"product": "B", "revenue": 20}),
    ];
    let opts = ChartOptions {
        chart_type: ChartType::Bar,
        x_column: Some("product".into()),
        y_column: Some("revenue".into()),
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.png");
    write_chart_to_disk(&path, &result.bytes, true).unwrap();

    let on_disk = std::fs::read(&path).unwrap();
    assert!(on_disk.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]));
}

/// Line chart with TIMESTAMPTZ-style labels auto-detects categorical mode
/// and renders without error. Labels with shared +00:00 suffix get shortened.
#[test]
fn line_chart_timestamptz_auto_categorical_and_label_shortening() {
    let rows = vec![
        json!({"ts": "2026-05-01 08:00:00+00:00", "value": 100}),
        json!({"ts": "2026-05-01 12:30:00+00:00", "value": 150}),
        json!({"ts": "2026-05-02 06:15:00+00:00", "value": 200}),
        json!({"ts": "2026-05-02 22:45:00+00:00", "value": 180}),
        json!({"ts": "2026-05-03 10:00:00+00:00", "value": 220}),
        json!({"ts": "2026-05-03 18:30:00+00:00", "value": 190}),
    ];
    let opts = ChartOptions {
        chart_type: ChartType::Line,
        x_column: Some("ts".into()),
        y_column: Some("value".into()),
        // x_as_category deliberately left as None to test auto-detection
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 6);
}

/// Many TIMESTAMPTZ labels auto-thin to avoid overlap.
#[test]
fn line_chart_many_timestamps_auto_thins() {
    let rows: Vec<_> = (0..30)
        .map(|i| {
            json!({
                "ts": format!("2026-05-{:02} 12:00:00+00:00", (i % 28) + 1),
                "value": i * 10
            })
        })
        .collect();
    let opts = ChartOptions {
        chart_type: ChartType::Line,
        x_column: Some("ts".into()),
        y_column: Some("value".into()),
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 30);
}

/// Regression: a 90-point hourly TIMESTAMP series used to render with
/// only ONE visible x-axis label because the old `shorten_labels`
/// blanked non-step indices and `plotters` picked tick positions that
/// rarely landed on a kept index. The fix tells `plotters` how many
/// ticks to draw up front, so every tick position carries a real label.
///
/// Uses SVG mode so we can inspect the rendered text content directly.
#[test]
fn line_chart_long_timestamp_series_renders_multiple_visible_labels() {
    let rows: Vec<_> = (0..90)
        .map(|i| {
            json!({
                "ts": format!("2026-01-{:02} {:02}:00:00", (i / 24) + 1, i % 24),
                "value": i
            })
        })
        .collect();
    let opts = ChartOptions {
        chart_type: ChartType::Line,
        format: ChartFormat::Svg,
        x_column: Some("ts".into()),
        y_column: Some("value".into()),
        width: 800,
        height: 480,
        ..ChartOptions::default()
    };
    let result = render_chart(&rows, &opts).unwrap();
    assert_eq!(result.rows_plotted, 90);
    let svg = String::from_utf8(result.bytes).expect("SVG must be UTF-8");
    // Each visible x-axis tick label is rendered as an SVG <text>
    // element containing the literal label string. Every label in this
    // series starts with "2026-01-", so counting that prefix gives the
    // number of *visible* x-axis labels (the y-axis labels are numeric
    // and won't match).
    let visible_labels = svg.matches("2026-01-").count();
    assert!(
        visible_labels >= 3,
        "expected >= 3 visible x-axis labels for a 90-point series, got {visible_labels} \
         (regression: pre-fix value was 1)"
    );
    // Also bound the upper end — too many would mean labels overlap;
    // 800 / (19chars * 7px + 10px) ≈ 5 is the heuristic target.
    assert!(
        visible_labels <= 12,
        "expected <= 12 visible labels (no overlap on 800px wide chart), got {visible_labels}"
    );
}
