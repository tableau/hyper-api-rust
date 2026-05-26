// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Chart rendering for query results.
//!
//! Converts a list of JSON rows (typically from [`crate::engine::Engine::execute_query_to_json`])
//! into a PNG or SVG image via the [`plotters`] crate. The output is raw bytes plus a
//! MIME type ready to drop into an MCP [`ImageContent`].
//!
//! # Supported Chart Types
//!
//! - **Bar** — categorical x-axis by default; multi-series supported via `series` column.
//! - **Line** — auto-detects categorical x (DATE/TIMESTAMP/TEXT); override with `x_as_category`.
//! - **Scatter** — same auto-detection as line.
//! - **Histogram** — single numeric column binned into N buckets (default 20).
//!
//! # Rendering Pipeline
//!
//! 1. The MCP `chart` tool runs a read-only SQL query via [`crate::engine::Engine`].
//! 2. Rows are grouped into series via `group_series` (categorical x values get
//!    synthetic sequential indices; numeric x values pass through directly).
//! 3. The chart is drawn on either a [`BitMapBackend`] (PNG, written to a temp file)
//!    or an [`SVGBackend`] (SVG, rendered to an in-memory string).
//! 4. The result is returned as base64-encoded [`ImageContent`] plus a JSON stats block.
//!
//! # Color Palette
//!
//! Multi-series charts cycle through an 8-color palette designed for white backgrounds.
//! The palette is defined in `series_color`.
//!
//! [`BitMapBackend`]: plotters::prelude::BitMapBackend
//! [`SVGBackend`]: plotters::prelude::SVGBackend
//! [`ImageContent`]: rmcp::model::ImageContent

#![allow(
    clippy::cast_precision_loss,
    reason = "chart rendering: rows/columns displayed to user; any values approaching 2^53 would saturate to Infinity in the chart anyway"
)]

use crate::error::{ErrorCode, McpError};
use plotters::prelude::*;
use plotters::style::colors;
use serde_json::Value;
use std::collections::BTreeMap;

/// A single chart series' data points.
///
/// Each entry is `(x, y, x_label)` where the numeric `x` drives
/// positioning on the axis and `x_label` preserves the original
/// string form of the x value so categorical axes can render
/// human-readable tick labels (the `group_series` function maps
/// category strings through a `BTreeMap<String, f64>` to assign
/// stable, deterministic x positions).
type SeriesPoints = Vec<(f64, f64, String)>;

/// Series name → its points. Uses `BTreeMap` (not `HashMap`) so
/// multi-series charts render in deterministic order, which makes
/// the resulting image bytes reproducible across runs.
type SeriesMap = BTreeMap<String, SeriesPoints>;

/// Supported chart types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartType {
    Bar,
    Line,
    Scatter,
    Histogram,
}

impl ChartType {
    /// Parse a string into a [`ChartType`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SchemaMismatch`] if `s` (case-insensitive) is
    /// not one of `bar`, `line`, `scatter`, `histogram`, or `hist`.
    pub fn parse(s: &str) -> Result<Self, McpError> {
        match s.to_lowercase().as_str() {
            "bar" => Ok(ChartType::Bar),
            "line" => Ok(ChartType::Line),
            "scatter" => Ok(ChartType::Scatter),
            "histogram" | "hist" => Ok(ChartType::Histogram),
            other => Err(McpError::new(
                ErrorCode::SchemaMismatch,
                format!(
                    "Unknown chart type '{other}'. Expected one of: bar, line, scatter, histogram"
                ),
            )),
        }
    }
}

/// Output format for the rendered chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartFormat {
    Png,
    Svg,
}

impl ChartFormat {
    /// Parse a string into a [`ChartFormat`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::UnsupportedFormat`] if `s` (case-insensitive)
    /// is not `png` or `svg`.
    pub fn parse(s: &str) -> Result<Self, McpError> {
        match s.to_lowercase().as_str() {
            "png" => Ok(ChartFormat::Png),
            "svg" => Ok(ChartFormat::Svg),
            other => Err(McpError::new(
                ErrorCode::UnsupportedFormat,
                format!("Unknown chart format '{other}'. Expected 'png' or 'svg'"),
            )),
        }
    }

    #[must_use]
    pub fn mime_type(&self) -> &'static str {
        match self {
            ChartFormat::Png => "image/png",
            ChartFormat::Svg => "image/svg+xml",
        }
    }

    /// File extension without leading dot (`"png"` / `"svg"`). Used when
    /// synthesizing default filenames under the system temp dir.
    #[must_use]
    pub fn extension(&self) -> &'static str {
        match self {
            ChartFormat::Png => "png",
            ChartFormat::Svg => "svg",
        }
    }
}

/// Resolve the effective output format from an explicit `format` parameter
/// and/or an `output_path`'s extension.
///
/// Rules:
/// - Both set: they must agree. Conflict returns `InvalidArgument` naming
///   both values so the caller can fix one.
/// - Only `format` set: parse it via [`ChartFormat::parse`].
/// - Only `output_path` set: derive from its extension (`.png` / `.svg`).
///   Unknown extensions return `InvalidArgument`.
/// - Neither set: default to PNG (matches the pre-change behavior).
///
/// The path is only inspected for its extension — the file need not exist.
///
/// # Errors
///
/// - Returns [`ErrorCode::InvalidArgument`] if both `explicit_format` and
///   `output_path` are set and they disagree on the format.
/// - Propagates [`ErrorCode::UnsupportedFormat`] from [`ChartFormat::parse`]
///   for unknown format strings.
/// - Returns [`ErrorCode::InvalidArgument`] (via `format_from_extension`)
///   when `output_path` has an extension other than `.png` or `.svg`.
pub fn resolve_chart_format(
    explicit_format: Option<&str>,
    output_path: Option<&str>,
) -> Result<ChartFormat, McpError> {
    let ext_from_path = output_path.and_then(extract_extension);

    match (explicit_format, ext_from_path.as_deref()) {
        (Some(f), Some(ext)) => {
            let from_format = ChartFormat::parse(f)?;
            let from_ext = format_from_extension(ext)?;
            if from_format != from_ext {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "chart: format=\"{f}\" conflicts with output_path extension \".{ext}\" — \
                         remove one or make them agree"
                    ),
                ));
            }
            Ok(from_format)
        }
        (Some(f), None) => ChartFormat::parse(f),
        (None, Some(ext)) => format_from_extension(ext),
        (None, None) => Ok(ChartFormat::Png),
    }
}

/// Lowercase extension of `path` with the leading dot stripped, or `None`
/// if the path has no extension or a non-UTF-8 extension.
fn extract_extension(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

/// Map a file extension (no leading dot, lowercased) to a `ChartFormat`.
/// Unknown extensions return `InvalidArgument` with a list of what's allowed.
fn format_from_extension(ext: &str) -> Result<ChartFormat, McpError> {
    match ext {
        "png" => Ok(ChartFormat::Png),
        "svg" => Ok(ChartFormat::Svg),
        other => Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!(
                "chart: unsupported output_path extension \".{other}\" (use .png or .svg, \
                 or omit output_path to auto-generate one)"
            ),
        )),
    }
}

/// How the `chart` tool should deliver the rendered image: write it to
/// disk, return it inline in the MCP tool result, or both. This is a
/// pure decision based on the caller's `inline` / `output_path` flags —
/// no I/O happens here; `write_chart_to_disk` does the actual write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChartDisposition {
    /// Write to `path`, don't return inline. Path is either caller-supplied
    /// or auto-generated under the system temp dir.
    WriteOnly { path: std::path::PathBuf },
    /// Return inline, don't write to disk.
    InlineOnly,
    /// Write to `path` and also return inline.
    WriteAndInline { path: std::path::PathBuf },
}

impl ChartDisposition {
    /// The target path, if any. `InlineOnly` has no path.
    #[must_use]
    pub fn path(&self) -> Option<&std::path::Path> {
        match self {
            ChartDisposition::WriteOnly { path } | ChartDisposition::WriteAndInline { path } => {
                Some(path)
            }
            ChartDisposition::InlineOnly => None,
        }
    }

    /// Whether to include `Content::image(...)` in the tool result.
    #[must_use]
    pub fn wants_inline(&self) -> bool {
        matches!(
            self,
            ChartDisposition::InlineOnly | ChartDisposition::WriteAndInline { .. }
        )
    }
}

/// Decide what the chart tool should do with the rendered bytes based on
/// the caller's `inline` and `output_path` flags plus the already-resolved
/// `format`.
///
/// Semantics (see the `chart` tool docs):
/// - `inline=true` + no path → `InlineOnly` (skip disk)
/// - `inline=true` + path    → `WriteAndInline` (both)
/// - `inline=false`/absent + path → `WriteOnly`
/// - `inline=false`/absent + no path → `WriteOnly` with auto-generated path
///   under `std::env::temp_dir()/hyperdb-charts/`
///
/// This is the default path most callers take: keeps the MCP transcript small
/// by writing the PNG/SVG to disk and letting the caller `Read(path)` when
/// they want to display it.
#[must_use]
pub fn resolve_chart_disposition(
    inline: bool,
    output_path: Option<&str>,
    format: ChartFormat,
) -> ChartDisposition {
    match (inline, output_path) {
        (true, None) => ChartDisposition::InlineOnly,
        (true, Some(p)) => ChartDisposition::WriteAndInline {
            path: std::path::PathBuf::from(p),
        },
        (false, Some(p)) => ChartDisposition::WriteOnly {
            path: std::path::PathBuf::from(p),
        },
        (false, None) => ChartDisposition::WriteOnly {
            path: auto_generated_chart_path(format),
        },
    }
}

/// Synthesize a unique path under `std::env::temp_dir()/hyperdb-charts/` for
/// a default-disposition chart write. The filename encodes a monotonic
/// counter + PID + unix-nanos so two calls in the same nanosecond (or on two
/// hosts with sync'd clocks) don't collide.
///
/// The parent directory is *not* created here — the caller does that right
/// before writing, to keep this function pure and cheap for testing.
pub fn auto_generated_chart_path(format: ChartFormat) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());

    std::env::temp_dir().join("hyperdb-charts").join(format!(
        "chart-{nanos}-{pid}-{counter}.{ext}",
        ext = format.extension()
    ))
}

/// Write chart bytes to `path`, creating the parent directory if needed and
/// honoring the `overwrite` flag.
///
/// Errors:
/// - `PermissionDenied` if `path` exists and `overwrite=false` (matches
///   `export`'s pre-flight check).
/// - `InternalError` wrapping the underlying `std::io::Error` for mkdir or
///   write failures.
///
/// Returns the number of bytes written.
///
/// # Errors
///
/// - Returns [`ErrorCode::PermissionDenied`] if `path` exists and
///   `overwrite` is `false`.
/// - Returns [`ErrorCode::InternalError`] wrapping the underlying
///   [`std::io::Error`] for `create_dir_all` or `write` failures.
pub fn write_chart_to_disk(
    path: &std::path::Path,
    bytes: &[u8],
    overwrite: bool,
) -> Result<u64, McpError> {
    // Reject `..` components to prevent traversal attacks via LLM-generated paths.
    // (We can't canonicalize a non-existent path, but rejecting `..` covers the
    // most common attack pattern.)
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!(
                "Chart output path '{}' may not contain '..' components",
                path.display()
            ),
        ));
    }

    if !overwrite && path.exists() {
        return Err(McpError::new(
            ErrorCode::PermissionDenied,
            format!(
                "Refusing to overwrite existing chart: {} (pass overwrite=true to replace it)",
                path.display()
            ),
        ));
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                McpError::new(
                    ErrorCode::InternalError,
                    format!(
                        "Failed to create parent directory for chart '{}': {e}",
                        path.display()
                    ),
                )
            })?;
        }
    }

    std::fs::write(path, bytes).map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("Failed to write chart to '{}': {e}", path.display()),
        )
    })?;

    Ok(bytes.len() as u64)
}

/// User-facing chart configuration, parsed from MCP tool parameters.
#[derive(Debug, Clone)]
pub struct ChartOptions {
    pub chart_type: ChartType,
    pub x_column: Option<String>,
    pub y_column: Option<String>,
    pub series_column: Option<String>,
    pub title: Option<String>,
    pub format: ChartFormat,
    pub width: u32,
    pub height: u32,
    pub bins: u32,
    /// Override the chart-type-specific default for how the x column is
    /// interpreted:
    ///
    /// - `None` (default): use the chart type's natural behavior — `Bar`
    ///   treats x as categorical, `Line` / `Scatter` require numeric x.
    /// - `Some(true)`: force categorical even on `Line` / `Scatter`.
    ///   Essential for plotting values whose natural axis is a string /
    ///   date / enum (e.g. `SELECT day, happiness_score` where `day` is a
    ///   `DATE`).
    /// - `Some(false)`: force numeric even on `Bar` (rarely useful — bar
    ///   charts are almost always categorical).
    ///
    /// When categorical mode is active the rendered x axis uses the
    /// original string representation of each distinct x value as its
    /// tick label, in the order x values are first seen.
    pub x_as_category: Option<bool>,
    /// Fix the x-axis range as `[min, max]`. When set, auto-scaling is
    /// skipped and all frames/charts share the same x extent. Useful for
    /// side-by-side comparisons or animation where a consistent scale
    /// matters. Ignored for bar charts (which use categorical positions).
    pub x_range: Option<[f64; 2]>,
    /// Fix the y-axis range as `[min, max]`. Same semantics as `x_range`.
    pub y_range: Option<[f64; 2]>,
    /// Map series names to hex colors (`"#rrggbb"`). Entries that match a
    /// series name override the default palette; unmatched series still
    /// cycle through palette colors. Only affects charts with a series
    /// column; single-series charts use the first palette color as before.
    pub color_map: std::collections::HashMap<String, RGBColor>,
    /// When `true`, draw the series name as a text label next to each dot
    /// on scatter (and each point on line) charts, and suppress the legend
    /// entirely. Useful when each series has exactly one point (e.g. one
    /// country per dot) and a legend would be redundant.
    ///
    /// Labels are drawn 6 pixels right and 4 pixels above the data point.
    /// No collision avoidance is performed — for dense data the legend
    /// (`label_points: false`, the default) is usually more readable.
    pub label_points: bool,
}

impl Default for ChartOptions {
    fn default() -> Self {
        Self {
            chart_type: ChartType::Bar,
            x_column: None,
            y_column: None,
            series_column: None,
            title: None,
            format: ChartFormat::Png,
            width: 800,
            height: 480,
            bins: 20,
            x_as_category: None,
            x_range: None,
            y_range: None,
            color_map: std::collections::HashMap::new(),
            label_points: false,
        }
    }
}

/// Result of rendering a chart.
#[derive(Debug)]
pub struct ChartResult {
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
    pub rows_plotted: usize,
}

/// Render a chart from a list of JSON row objects.
///
/// `rows` is expected to be the output of `execute_query_to_json`: each entry
/// is a `Value::Object` with column name → value pairs. Non-object rows are
/// skipped silently.
///
/// # Errors
///
/// - Returns [`ErrorCode::EmptyData`] if `rows` is empty.
/// - Returns [`ErrorCode::SchemaMismatch`] if required columns named in
///   `opts` are absent, if x or y columns cannot be interpreted as
///   numeric for chart types that require numeric axes, or if a
///   categorical axis produces zero distinct categories.
/// - Returns [`ErrorCode::InternalError`] wrapping failures from the
///   underlying `plotters` backend during rendering or PNG/SVG encoding.
/// - Returns [`ErrorCode::InvalidArgument`] if the result set exceeds
///   50,000 rows.
pub fn render_chart(rows: &[Value], opts: &ChartOptions) -> Result<ChartResult, McpError> {
    const MAX_CHART_ROWS: usize = 50_000;
    if rows.is_empty() {
        return Err(McpError::new(
            ErrorCode::EmptyData,
            "No rows returned from SQL query — nothing to chart",
        ));
    }
    if rows.len() > MAX_CHART_ROWS {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!(
                "Chart data has {} rows, exceeding the {MAX_CHART_ROWS}-row limit. \
                 Add a LIMIT clause or aggregate your data to reduce row count.",
                rows.len()
            ),
        )
        .with_suggestion(format!(
            "Add `LIMIT {MAX_CHART_ROWS}` to your query, or use GROUP BY to aggregate."
        )));
    }

    match opts.format {
        ChartFormat::Png => render_png(rows, opts),
        ChartFormat::Svg => render_svg(rows, opts),
    }
}

fn render_png(rows: &[Value], opts: &ChartOptions) -> Result<ChartResult, McpError> {
    let tmp = tempfile::Builder::new()
        .suffix(".png")
        .tempfile()
        .map_err(|e| {
            McpError::new(
                ErrorCode::InternalError,
                format!("Cannot create temp PNG file: {e}"),
            )
        })?;
    let path = tmp.path().to_path_buf();
    let rows_plotted = {
        let backend = BitMapBackend::new(&path, (opts.width, opts.height));
        draw_on_backend(backend, rows, opts)?
    };
    let bytes = std::fs::read(&path).map_err(|e| {
        McpError::new(
            ErrorCode::InternalError,
            format!("Cannot read rendered PNG: {e}"),
        )
    })?;
    drop(tmp);
    Ok(ChartResult {
        bytes,
        mime_type: ChartFormat::Png.mime_type(),
        rows_plotted,
    })
}

fn render_svg(rows: &[Value], opts: &ChartOptions) -> Result<ChartResult, McpError> {
    let mut svg_string = String::new();
    let rows_plotted = {
        let backend = SVGBackend::with_string(&mut svg_string, (opts.width, opts.height));
        draw_on_backend(backend, rows, opts)?
    };
    Ok(ChartResult {
        bytes: svg_string.into_bytes(),
        mime_type: ChartFormat::Svg.mime_type(),
        rows_plotted,
    })
}

/// Dispatch to the chart-type-specific drawing routine over an abstract backend.
fn draw_on_backend<DB: DrawingBackend>(
    backend: DB,
    rows: &[Value],
    opts: &ChartOptions,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let root = backend.into_drawing_area();
    root.fill(&WHITE).map_err(draw_err)?;

    match opts.chart_type {
        ChartType::Bar => draw_bar(&root, rows, opts),
        ChartType::Line => draw_line(&root, rows, opts),
        ChartType::Scatter => draw_scatter(&root, rows, opts),
        ChartType::Histogram => draw_histogram(&root, rows, opts),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "call-site ergonomics: function consumes logically-owned parameters, refactoring signatures is not worth per-site churn"
)]
fn draw_err<E: std::error::Error + Send + Sync + 'static>(e: DrawingAreaErrorKind<E>) -> McpError {
    McpError::new(
        ErrorCode::InternalError,
        format!("Chart rendering error: {e}"),
    )
}

#[expect(
    clippy::ref_option,
    reason = "matches callers that already hold `&Option<T>`; avoiding a `.as_ref()` dance at every call site"
)]
fn require_column<'a>(col: &'a Option<String>, role: &str) -> Result<&'a str, McpError> {
    col.as_deref().ok_or_else(|| {
        McpError::new(
            ErrorCode::SchemaMismatch,
            format!("The '{role}' column name is required for this chart type"),
        )
    })
}

fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn as_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Shorten categorical tick labels for display. Two passes:
///
/// 1. **Strip shared timezone offset** — if every label ends with the
///    same `+HH:MM` or `+00:00` (common for TIMESTAMPTZ), drop that
///    suffix since it adds no information.
/// 2. **Auto-thin** — if there are more labels than can fit without
///    overlap (estimated at `chart_width / (avg_char_count * 7px)`),
///    keep only every Nth label and blank the rest.
fn shorten_labels(labels: &[String], chart_width: u32) -> Vec<String> {
    if labels.is_empty() {
        return Vec::new();
    }
    // Pass 1: strip shared timezone suffix (e.g. "+00:00", "+05:30")
    let stripped: Vec<String> = if labels.len() > 1 {
        let suffix = shared_tz_suffix(labels);
        if let Some(ref sfx) = suffix {
            labels
                .iter()
                .map(|l| l.strip_suffix(sfx.as_str()).unwrap_or(l).trim().to_string())
                .collect()
        } else {
            labels.to_vec()
        }
    } else {
        labels.to_vec()
    };

    // Pass 2: auto-thin if labels would overlap
    let max_len = stripped.iter().map(String::len).max().unwrap_or(1);
    let char_px = 7_u32;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "label length is bounded by real-world string sizes (< 200 chars)"
    )]
    let label_px = (max_len as u32) * char_px + 10;
    let fits = (chart_width / label_px.max(1)) as usize;
    if fits >= stripped.len() || stripped.len() <= 2 {
        return stripped;
    }
    // Show every Nth label, always including first and last
    let step = (stripped.len() + fits - 1) / fits.max(1);
    stripped
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if i == 0 || i == stripped.len() - 1 || i % step == 0 {
                l.clone()
            } else {
                String::new()
            }
        })
        .collect()
}

/// If all labels share a trailing timezone offset pattern like `+00:00`
/// or `-05:30`, return that suffix. Returns `None` if labels differ or
/// have no offset.
fn shared_tz_suffix(labels: &[String]) -> Option<String> {
    let first = labels.first()?;
    // Match pattern: space or 'T' followed by time, then +/-HH:MM at the end
    let offset_start = first.rfind('+').or_else(|| {
        // Careful: don't match the '-' in "2026-05-01"
        let last_minus = first.rfind('-')?;
        // Only if it's after a ':' (i.e. part of time, not date)
        if first[..last_minus].ends_with(|c: char| c.is_ascii_digit()) && last_minus > 10 {
            Some(last_minus)
        } else {
            None
        }
    })?;
    let suffix = &first[offset_start..];
    // Must look like +HH:MM or -HH:MM (6 chars)
    if suffix.len() != 6 {
        return None;
    }
    // Verify all labels share this suffix
    if labels.iter().all(|l| l.ends_with(suffix)) {
        Some(suffix.to_string())
    } else {
        None
    }
}

/// Collect distinct x values and their original string labels from a
/// [`SeriesMap`], in ascending x-value order.
///
/// Used by [`draw_bar`] (always) and by [`draw_line_or_scatter`] when
/// `x_as_category=true`. The returned (`x_val`, label) pairs drive the
/// `x_label_formatter` that renders axis ticks as strings — essential
/// for charts over `DATE` / enum / name-keyed data where `x_val` is a
/// synthetic sequential index assigned by `group_series`'s category
/// mode rather than a meaningful number.
fn collect_categories(groups: &SeriesMap) -> Vec<(f64, String)> {
    // Dedup by bit pattern so NaN handling stays consistent with how
    // `BTreeMap<f64>` would behave (we store as `u64` bits because
    // `f64: !Ord`). The final sort is by numeric value.
    let mut seen: BTreeMap<u64, String> = BTreeMap::new();
    for pts in groups.values() {
        for (x, _y, label) in pts {
            seen.entry(x.to_bits()).or_insert_with(|| label.clone());
        }
    }
    let mut entries: Vec<_> = seen.into_iter().collect();
    entries.sort_by(|a, b| {
        f64::from_bits(a.0)
            .partial_cmp(&f64::from_bits(b.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entries
        .into_iter()
        .map(|(bits, label)| (f64::from_bits(bits), label))
        .collect()
}

/// Group rows into (`series_name`, points) buckets, extracting x and y values.
/// When `series_col` is None, all points land in a single unnamed series.
fn group_series(
    rows: &[Value],
    x_col: &str,
    y_col: &str,
    series_col: Option<&str>,
    x_as_category: bool,
) -> Result<SeriesMap, McpError> {
    let mut groups: SeriesMap = BTreeMap::new();
    let mut category_index: BTreeMap<String, f64> = BTreeMap::new();

    for row in rows {
        let Some(obj) = row.as_object() else { continue };

        let y_val = obj.get(y_col).and_then(as_number).ok_or_else(|| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Column '{y_col}' is missing or not numeric in at least one row"),
            )
        })?;

        let x_raw = obj.get(x_col).cloned().unwrap_or(Value::Null);
        let x_label = as_string(&x_raw);
        let x_val = if x_as_category {
            let next = category_index.len() as f64;
            *category_index.entry(x_label.clone()).or_insert(next)
        } else {
            as_number(&x_raw).ok_or_else(|| {
                McpError::new(
                    ErrorCode::SchemaMismatch,
                    format!("Column '{x_col}' is missing or not numeric in at least one row"),
                )
            })?
        };

        let series_key = match series_col {
            Some(s) => obj.get(s).map(as_string).unwrap_or_default(),
            None => String::new(),
        };

        groups
            .entry(series_key)
            .or_default()
            .push((x_val, y_val, x_label));
    }

    if groups.values().all(std::vec::Vec::is_empty) {
        return Err(McpError::new(
            ErrorCode::EmptyData,
            "No valid data points after filtering",
        ));
    }

    Ok(groups)
}

/// Pick a color from the palette by index, cycling as needed.
fn series_color(idx: usize) -> RGBColor {
    // 8 distinct colors that work on white background; cycles for more series.
    const PALETTE: [RGBColor; 8] = [
        RGBColor(31, 119, 180),  // muted blue
        RGBColor(255, 127, 14),  // safety orange
        RGBColor(44, 160, 44),   // cooked asparagus
        RGBColor(214, 39, 40),   // brick red
        RGBColor(148, 103, 189), // muted purple
        RGBColor(140, 86, 75),   // chestnut brown
        RGBColor(227, 119, 194), // raspberry yogurt pink
        RGBColor(127, 127, 127), // middle gray
    ];
    PALETTE[idx % PALETTE.len()]
}

/// Resolve the color for `series_name`: check `color_map` first, fall back
/// to the palette-by-index default so unmapped series still get a color.
fn series_color_for(series_name: &str, idx: usize, opts: &ChartOptions) -> RGBColor {
    opts.color_map
        .get(series_name)
        .copied()
        .unwrap_or_else(|| series_color(idx))
}

/// Parse a `"#rrggbb"` hex string into an `RGBColor`. Returns `None` when
/// the string is not in the expected format so callers can log and skip
/// rather than hard-failing.
#[must_use]
pub fn parse_hex_color(s: &str) -> Option<RGBColor> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(RGBColor(r, g, b))
}

fn draw_bar<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    rows: &[Value],
    opts: &ChartOptions,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let x_col = require_column(&opts.x_column, "x")?;
    let y_col = require_column(&opts.y_column, "y")?;

    // Bar charts default to categorical x axis; `ChartOptions::x_as_category`
    // lets callers force numeric if they really want to.
    let x_as_category = opts.x_as_category.unwrap_or(true);
    let groups = group_series(
        rows,
        x_col,
        y_col,
        opts.series_column.as_deref(),
        x_as_category,
    )?;

    let categories = collect_categories(&groups);

    let x_min = -0.5_f64;
    let x_max = categories.len() as f64 - 0.5;

    let y_min = groups
        .values()
        .flat_map(|pts| pts.iter().map(|(_, y, _)| *y))
        .fold(f64::INFINITY, f64::min)
        .min(0.0);
    let y_max = groups
        .values()
        .flat_map(|pts| pts.iter().map(|(_, y, _)| *y))
        .fold(f64::NEG_INFINITY, f64::max)
        .max(0.0);
    let y_pad = (y_max - y_min).abs() * 0.1 + 1.0;

    let title = opts
        .title
        .clone()
        .unwrap_or_else(|| format!("{y_col} by {x_col}"));

    let mut chart = ChartBuilder::on(root)
        .caption(&title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(60)
        .y_label_area_size(70)
        .build_cartesian_2d(x_min..x_max, (y_min - y_pad)..(y_max + y_pad))
        .map_err(draw_err)?;

    let raw_labels: Vec<String> = categories.iter().map(|(_, l)| l.clone()).collect();
    let labels = shorten_labels(&raw_labels, opts.width);
    chart
        .configure_mesh()
        .x_labels(categories.len().min(20))
        .x_label_formatter(&|v| {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "axis tick value originated as an integer index into `labels`; the subsequent `usize::try_from` + length check make out-of-range ticks render as the empty-string branch"
            )]
            let idx = v.round() as isize;
            usize::try_from(idx)
                .ok()
                .and_then(|i| labels.get(i).cloned())
                .unwrap_or_default()
        })
        .y_desc(y_col)
        .x_desc(x_col)
        .draw()
        .map_err(draw_err)?;

    let num_series = groups.len().max(1);
    let total_width = 0.8_f64;
    let bar_width = total_width / num_series as f64;
    let mut total_plotted = 0usize;
    for (idx, (series_key, pts)) in groups.iter().enumerate() {
        let color = series_color_for(series_key, idx, opts);
        let offset = -total_width / 2.0 + bar_width * (idx as f64 + 0.5);
        let name = if series_key.is_empty() {
            y_col.to_string()
        } else {
            series_key.clone()
        };
        chart
            .draw_series(pts.iter().map(|(x, y, _)| {
                let left = x + offset - bar_width / 2.0;
                let right = x + offset + bar_width / 2.0;
                Rectangle::new([(left, 0.0), (right, *y)], color.filled())
            }))
            .map_err(draw_err)?
            .label(name)
            .legend(move |(x, y)| Rectangle::new([(x, y - 5), (x + 12, y + 5)], color.filled()));
        total_plotted += pts.len();
    }

    chart
        .configure_series_labels()
        .background_style(colors::WHITE.mix(0.9))
        .border_style(colors::BLACK)
        .draw()
        .map_err(draw_err)?;

    root.present().map_err(draw_err)?;
    Ok(total_plotted)
}

fn draw_line<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    rows: &[Value],
    opts: &ChartOptions,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    line_or_scatter(root, rows, opts, true)
}

fn draw_scatter<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    rows: &[Value],
    opts: &ChartOptions,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    line_or_scatter(root, rows, opts, false)
}

#[expect(
    clippy::similar_names,
    reason = "paired bindings (request/response, reader/writer, etc.) are more readable with symmetric names than artificially distinct ones"
)]
/// Shared implementation for line and scatter charts. `connect_points` controls
/// whether successive points are joined with a line.
fn line_or_scatter<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    rows: &[Value],
    opts: &ChartOptions,
    connect_points: bool,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let x_col = require_column(&opts.x_column, "x")?;
    let y_col = require_column(&opts.y_column, "y")?;
    // Auto-detect categorical x: if the caller didn't explicitly set
    // x_as_category and the first row's x value isn't numeric (e.g.
    // DATE, TIMESTAMP, TEXT), flip to categorical mode automatically.
    let x_as_category = opts.x_as_category.unwrap_or_else(|| {
        rows.first()
            .and_then(Value::as_object)
            .and_then(|obj| obj.get(x_col))
            .is_some_and(|v| as_number(v).is_none())
    });
    let groups = group_series(
        rows,
        x_col,
        y_col,
        opts.series_column.as_deref(),
        x_as_category,
    )?;

    let auto = bounds(&groups);
    let (rx_min, rx_max, ry_min, ry_max) = apply_ranges(auto, opts);

    let default_title = if connect_points {
        "Line chart"
    } else {
        "Scatter plot"
    };
    let title = opts.title.clone().unwrap_or_else(|| default_title.into());

    let mut chart = ChartBuilder::on(root)
        .caption(&title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(if x_as_category { 60 } else { 50 })
        .y_label_area_size(70)
        .build_cartesian_2d(rx_min..rx_max, ry_min..ry_max)
        .map_err(draw_err)?;

    // In categorical mode the x values are synthetic sequential indices
    // assigned by `group_series` — the axis ticks need a formatter that
    // translates the index back to the original string label, otherwise
    // the rendered chart would show 0, 1, 2, ... where a reader expects
    // dates or names.
    if x_as_category {
        let categories = collect_categories(&groups);
        let raw_labels: Vec<String> = categories.iter().map(|(_, l)| l.clone()).collect();
        let labels = shorten_labels(&raw_labels, opts.width);
        chart
            .configure_mesh()
            .x_desc(x_col)
            .y_desc(y_col)
            .x_labels(categories.len().min(20))
            .x_label_formatter(&|v| {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "axis tick value originated as an integer index into `labels`; the subsequent `usize::try_from` + length check make out-of-range ticks render as the empty-string branch"
                )]
                let idx = v.round() as isize;
                usize::try_from(idx)
                    .ok()
                    .and_then(|i| labels.get(i).cloned())
                    .unwrap_or_default()
            })
            .draw()
            .map_err(draw_err)?;
    } else {
        chart
            .configure_mesh()
            .x_desc(x_col)
            .y_desc(y_col)
            .draw()
            .map_err(draw_err)?;
    }

    let mut total_plotted = 0usize;
    for (idx, (series_key, pts)) in groups.iter().enumerate() {
        let color = series_color_for(series_key, idx, opts);
        let name = if series_key.is_empty() {
            y_col.to_string()
        } else {
            series_key.clone()
        };
        let mut sorted = pts.clone();
        if connect_points {
            sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        }

        if opts.label_points {
            // Draw dots/lines without registering a legend entry, then
            // annotate each point with the series name as a text label.
            if connect_points {
                chart
                    .draw_series(LineSeries::new(
                        sorted.iter().map(|(x, y, _)| (*x, *y)),
                        color.stroke_width(2),
                    ))
                    .map_err(draw_err)?;
            } else {
                chart
                    .draw_series(
                        sorted
                            .iter()
                            .map(|(x, y, _)| Circle::new((*x, *y), 4, color.filled())),
                    )
                    .map_err(draw_err)?;
            }
            // Text label offset: right+above by default. When the dot is in
            // the right 25% of the x range, flip the label left so it stays
            // inside the chart area. When near the bottom 15% of y, flip up
            // so the label isn't below the axis line.
            let x_flip_threshold = rx_min + (rx_max - rx_min) * 0.75;
            let y_flip_threshold = ry_min + (ry_max - ry_min) * 0.15;
            let label_style = ("sans-serif", 11).into_font().color(&BLACK);
            chart
                .draw_series(sorted.iter().map(|(x, y, _)| {
                    let label = name.clone();
                    // Estimate pixel width: ~7px per Unicode character for 11pt font.
                    // This is still approximate but handles multi-byte UTF-8 correctly.
                    //
                    // Series label lengths in MCP outputs are bounded well under
                    // 10k characters; saturating at `i32::MAX` is the right
                    // behavior for a pixel offset anyway — anything larger
                    // would already be off-canvas.
                    let char_px = i32::try_from(label.chars().count())
                        .unwrap_or(i32::MAX)
                        .saturating_mul(7);
                    let x_off = if *x >= x_flip_threshold {
                        -(char_px + 6)
                    } else {
                        6
                    };
                    let y_off = if *y <= y_flip_threshold { -20 } else { -12 };
                    EmptyElement::at((*x, *y))
                        + Text::new(label, (x_off, y_off), label_style.clone())
                }))
                .map_err(draw_err)?;
        } else {
            // Default: dots/lines with legend entry.
            if connect_points {
                chart
                    .draw_series(LineSeries::new(
                        sorted.iter().map(|(x, y, _)| (*x, *y)),
                        color.stroke_width(2),
                    ))
                    .map_err(draw_err)?
                    .label(name)
                    .legend(move |(x, y)| {
                        PathElement::new(vec![(x, y), (x + 16, y)], color.stroke_width(2))
                    });
            } else {
                chart
                    .draw_series(
                        sorted
                            .iter()
                            .map(|(x, y, _)| Circle::new((*x, *y), 4, color.filled())),
                    )
                    .map_err(draw_err)?
                    .label(name)
                    .legend(move |(x, y)| Circle::new((x + 8, y), 4, color.filled()));
            }
        }
        total_plotted += pts.len();
    }

    // Only draw the legend box when label_points is off — with labels
    // on the dots, the legend is redundant and takes up chart space.
    if !opts.label_points {
        chart
            .configure_series_labels()
            .background_style(colors::WHITE.mix(0.9))
            .border_style(colors::BLACK)
            .draw()
            .map_err(draw_err)?;
    }

    root.present().map_err(draw_err)?;
    Ok(total_plotted)
}

fn bounds(groups: &SeriesMap) -> (f64, f64, f64, f64) {
    let (mut x_min, mut x_max) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut y_min, mut y_max) = (f64::INFINITY, f64::NEG_INFINITY);
    for pts in groups.values() {
        for (x, y, _) in pts {
            if *x < x_min {
                x_min = *x;
            }
            if *x > x_max {
                x_max = *x;
            }
            if *y < y_min {
                y_min = *y;
            }
            if *y > y_max {
                y_max = *y;
            }
        }
    }
    if !x_min.is_finite() {
        x_min = 0.0;
    }
    if !x_max.is_finite() {
        x_max = 1.0;
    }
    if !y_min.is_finite() {
        y_min = 0.0;
    }
    if !y_max.is_finite() {
        y_max = 1.0;
    }
    if (x_max - x_min).abs() < 1e-12 {
        x_max = x_min + 1.0;
    }
    if (y_max - y_min).abs() < 1e-12 {
        y_max = y_min + 1.0;
    }
    (x_min, x_max, y_min, y_max)
}

#[expect(
    clippy::similar_names,
    reason = "paired bindings (request/response, reader/writer, etc.) are more readable with symmetric names than artificially distinct ones"
)]
/// Apply optional fixed-range overrides from `ChartOptions`, returning the
/// final `(x_min, x_max, y_min, y_max)` to pass to `build_cartesian_2d`.
///
/// When a range is provided the auto-computed bound is replaced entirely —
/// no padding is added on the overridden axes. Auto-computed axes still
/// receive their normal 5% padding so they don't clip the outermost point.
fn apply_ranges(auto: (f64, f64, f64, f64), opts: &ChartOptions) -> (f64, f64, f64, f64) {
    let (x_min, x_max, y_min, y_max) = auto;
    let x_pad = (x_max - x_min).abs() * 0.05 + 1e-9;
    let y_pad = (y_max - y_min).abs() * 0.05 + 1e-9;
    let (final_x_min, final_x_max) = match opts.x_range {
        Some([lo, hi]) => (lo, hi),
        None => (x_min - x_pad, x_max + x_pad),
    };
    let (final_y_min, final_y_max) = match opts.y_range {
        Some([lo, hi]) => (lo, hi),
        None => (y_min - y_pad, y_max + y_pad),
    };
    (final_x_min, final_x_max, final_y_min, final_y_max)
}

fn draw_histogram<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    rows: &[Value],
    opts: &ChartOptions,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    // Histograms use a single numeric column. Prefer x_column, fall back to y_column.
    let col = opts
        .x_column
        .as_deref()
        .or(opts.y_column.as_deref())
        .ok_or_else(|| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                "Histogram requires an 'x' or 'y' column name",
            )
        })?;

    let values: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.as_object().and_then(|o| o.get(col)).and_then(as_number))
        .collect();
    if values.is_empty() {
        return Err(McpError::new(
            ErrorCode::SchemaMismatch,
            format!("Column '{col}' has no numeric values to histogram"),
        ));
    }

    let bin_count = opts.bins.max(1) as usize;
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = if (max - min).abs() < 1e-12 {
        1.0
    } else {
        max - min
    };
    let bin_width = span / bin_count as f64;

    let mut bins = vec![0u64; bin_count];
    for v in &values {
        // Histogram bin index: `floor((v - min) / bin_width)` is finite and
        // lies in `[0, bin_count)` for well-formed inputs; we still clamp
        // with `.max(0).min(bin_count - 1)` to defend against NaN/rounding.
        // The narrowing to `isize` / `usize` is therefore a reinterpret of a
        // value we have just bounded to a small non-negative integer.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "bin index is clamped into `[0, bin_count)` on the surrounding lines, so the narrowing f64→isize→usize is a reinterpret of an already-bounded small integer"
        )]
        let idx = (((*v - min) / bin_width).floor() as isize).max(0) as usize;
        let idx = idx.min(bin_count - 1);
        bins[idx] += 1;
    }

    let y_max = *bins.iter().max().unwrap_or(&1) as f64;
    let title = opts
        .title
        .clone()
        .unwrap_or_else(|| format!("Distribution of {col}"));

    let mut chart = ChartBuilder::on(root)
        .caption(&title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(50)
        .y_label_area_size(60)
        .build_cartesian_2d(min..(max + bin_width * 0.01), 0.0..(y_max * 1.1 + 1.0))
        .map_err(draw_err)?;

    chart
        .configure_mesh()
        .x_desc(col)
        .y_desc("count")
        .draw()
        .map_err(draw_err)?;

    let color = series_color(0);
    chart
        .draw_series(bins.iter().enumerate().map(|(i, count)| {
            let left = min + bin_width * i as f64;
            let right = left + bin_width;
            Rectangle::new([(left, 0.0), (right, *count as f64)], color.filled())
        }))
        .map_err(draw_err)?;

    root.present().map_err(draw_err)?;
    Ok(values.len())
}
