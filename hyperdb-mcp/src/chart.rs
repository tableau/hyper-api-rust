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
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
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
    /// - `None` (default): auto-detect from the first row's x value.
    ///   - For `Bar`: always categorical.
    ///   - For `Line` / `Scatter`: numeric x → numeric axis;
    ///     DATE / TIMESTAMP / TIMESTAMPTZ string → **proportional time
    ///     axis** (positions are real Unix epoch seconds, ticks formatted
    ///     in the matching kind); TEXT → categorical fallback.
    /// - `Some(true)`: force categorical layout (synthetic sequential
    ///   x positions, original strings as tick labels). Useful when you
    ///   want even spacing on temporal data — e.g. one bar per business
    ///   day with no visual gap for weekends.
    /// - `Some(false)`: force numeric x. Errors for non-numeric inputs
    ///   on `Line` / `Scatter`. Rarely useful on `Bar`.
    ///
    /// When categorical mode is active the rendered x axis uses the
    /// original string representation of each distinct x value as its
    /// tick label, in the order x values are first seen. When time mode
    /// is active, gaps between data points reflect real wall-clock time
    /// rather than insertion order.
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

/// Compact categorical tick labels by stripping a shared trailing
/// timezone offset, when every label ends with the same one (typical
/// for TIMESTAMPTZ data stored in UTC where every row reports `+00:00`).
///
/// Returns labels unchanged when there's no shared suffix or fewer than
/// two labels. Tick *count* selection lives in [`auto_tick_count`]; this
/// pass is purely about removing redundant characters from each label.
fn strip_shared_tz_suffix(labels: &[String]) -> Vec<String> {
    if labels.len() <= 1 {
        return labels.to_vec();
    }
    let Some(suffix) = shared_tz_suffix(labels) else {
        return labels.to_vec();
    };
    labels
        .iter()
        .map(|l| {
            l.strip_suffix(suffix.as_str())
                .unwrap_or(l)
                .trim()
                .to_string()
        })
        .collect()
}

/// Decide how many tick labels `plotters` should draw on a categorical
/// x-axis given the labels we plan to render and the chart pixel width.
///
/// We pass the result to `.x_labels(N)` so `plotters` distributes tick
/// positions across the categorical range. The formatter then renders
/// the *real* label at each position — never blanks — so the user sees
/// a usable, evenly-spaced subset rather than a sea of empty strings.
///
/// Heuristic: estimate per-label pixel width as
/// `max_label_chars * 7px + 10px` (close to plotters' default mesh
/// font), divide the chart width by that, then clamp to
/// `[2, labels.len()]`. Returns `labels.len()` directly when there
/// are 0 or 1 labels.
///
/// # Why not blank labels at non-step indices?
///
/// `plotters` picks its own tick *positions* on the float axis (e.g.
/// `0.0, 4.7, 9.4, …` for a 0..89 categorical range). Rounding those
/// back to integer indices rarely lands on the same indices a "keep
/// every Nth, blank the rest" rule would preserve, so most ticks
/// would render as empty strings. Telling `plotters` how many ticks
/// to draw and always returning a real label is the only stable fix.
///
/// # Caveat: `plotters` rounds down to the next "nice" subdivision
///
/// `plotters::compute_f64_key_points` picks the smallest scale (most
/// ticks) such that `npoints ≤ max_points`, drawing scales from a
/// fixed band table `{1, 2, 5, 10, 20, 50, 100, …}`. So a wider chart
/// requesting 9 ticks across a 0..89 range still ends up with 5 ticks
/// (band 20), because the next denser band gives 10 ticks > 9. The
/// fix for that is *not* to multiply the request — at 800 px width 10
/// labels of 19 chars each (1430 px) would overlap. The proper fix
/// for time-series charts is the proportional time-axis path, where
/// `plotters` picks nice time intervals against real epoch positions
/// and the band-rounding artifact disappears entirely.
fn auto_tick_count(labels: &[String], chart_width: u32) -> usize {
    if labels.len() <= 1 {
        return labels.len();
    }
    let max_chars = labels.iter().map(|l| l.chars().count()).max().unwrap_or(1);
    tick_count_for_label_width(max_chars, chart_width).min(labels.len())
}

/// Compute how many tick labels can fit horizontally, given a typical
/// label character count and the chart pixel width. Pure width math —
/// no clamping against a label count or label slice. Use this when
/// the actual label list isn't available up front (e.g. the temporal
/// branch generates labels lazily inside the formatter closure).
///
/// Returns at least 2 so the axis stays informative even when labels
/// would technically overlap.
fn tick_count_for_label_width(label_chars: usize, chart_width: u32) -> usize {
    let per_label_px = u32::try_from(label_chars)
        .unwrap_or(u32::MAX)
        .saturating_mul(7)
        .saturating_add(10);
    let fits = chart_width.saturating_div(per_label_px.max(1)) as usize;
    fits.max(2)
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

/// Discriminator for temporal x-axis input formats. Drives both the
/// date parser ([`parse_temporal`]) and the time-axis label formatter,
/// so a chart with `DATE` x values doesn't waste pixels on `00:00:00`
/// suffixes and a `TIMESTAMPTZ` chart preserves its timezone offset on
/// rendered tick labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemporalKind {
    /// `YYYY-MM-DD` — labels rendered as `%Y-%m-%d`, ticks land at
    /// midnight UTC.
    Date,
    /// `YYYY-MM-DD HH:MM:SS` — labels rendered as `%Y-%m-%d %H:%M:%S`,
    /// positioned at their face-value UTC equivalent (TIMESTAMP is
    /// timezone-naive by definition).
    DateTime,
    /// `YYYY-MM-DD HH:MM:SS+HH:MM` — wrapped offset is the seconds
    /// east of UTC parsed from the *first* row. Subsequent rows are
    /// positioned in true UTC and re-rendered in this offset's local
    /// time, so a chart over uniformly-`+00:00` data displays UTC
    /// labels and a chart over `+05:30` data displays IST.
    DateTimeTz(i32),
}

/// How to interpret the x column when extracting f64 axis positions.
///
/// Drives [`group_series`] and the corresponding rendering branch in
/// [`line_or_scatter`] / [`draw_bar`]. `Temporal` is the new mode added
/// for proportional time-axis rendering: x positions are real Unix
/// epoch seconds (so 6 hours apart on the wire are 6 hours apart on
/// the chart), and tick labels are formatted via chrono.
#[derive(Debug, Clone, Copy)]
enum XMode {
    /// X values must be JSON numbers; positions pass through directly.
    Numeric,
    /// X values are stringified and assigned synthetic sequential
    /// indices in first-seen order. All positions are integers, so
    /// gaps in real-world spacing are flattened.
    Categorical,
    /// X values are parsed as temporal strings and positioned at their
    /// Unix epoch seconds. Spacing is proportional to real time; tick
    /// labels use a chrono format derived from the detected `kind`.
    Temporal(TemporalKind),
}

/// Parse a SQL temporal string ([`Value`] of `String` shape) into
/// `(kind, epoch_seconds_as_f64)`. Returns `None` when the value isn't
/// a recognized DATE / TIMESTAMP / TIMESTAMPTZ form.
///
/// Recognized formats (most-specific first):
/// - `YYYY-MM-DD HH:MM:SS+HH:MM` and `T` separator → [`TemporalKind::DateTimeTz`]
/// - `YYYY-MM-DD HH:MM:SS+HHMM` (no colon in offset)
/// - `YYYY-MM-DD HH:MM:SS` (and fractional seconds) → [`TemporalKind::DateTime`]
/// - `YYYY-MM-DD HH:MM` (no seconds) → [`TemporalKind::DateTime`]
/// - `YYYY-MM-DD` → [`TemporalKind::Date`]
///
/// `DateTime` strings are treated as UTC for positioning purposes —
/// they're naive by definition, so we have no other choice. The label
/// formatter will reproduce the input format faithfully.
fn parse_temporal(s: &str) -> Option<(TemporalKind, f64)> {
    const TZ_FORMATS: &[&str] = &[
        "%Y-%m-%d %H:%M:%S%:z",
        "%Y-%m-%dT%H:%M:%S%:z",
        "%Y-%m-%d %H:%M:%S%z",
        "%Y-%m-%dT%H:%M:%S%z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%:z",
    ];
    for fmt in TZ_FORMATS {
        if let Ok(dt) = DateTime::<FixedOffset>::parse_from_str(s, fmt) {
            let offset = dt.offset().local_minus_utc();
            return Some((TemporalKind::DateTimeTz(offset), dt.timestamp() as f64));
        }
    }

    const DT_FORMATS: &[&str] = &[
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ];
    for fmt in DT_FORMATS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some((
                TemporalKind::DateTime,
                Utc.from_utc_datetime(&dt).timestamp() as f64,
            ));
        }
    }

    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0)?;
        return Some((
            TemporalKind::Date,
            Utc.from_utc_datetime(&dt).timestamp() as f64,
        ));
    }

    None
}

/// Decide the x mode for a line/scatter chart from the first row's
/// x value. Used when the caller didn't explicitly set `x_as_category`.
///
/// Priority:
/// 1. Numeric (JSON number) → [`XMode::Numeric`].
/// 2. String parsing as DATE/TIMESTAMP/TIMESTAMPTZ → [`XMode::Temporal`].
/// 3. Anything else (TEXT, missing) → [`XMode::Categorical`] fallback.
fn detect_line_x_mode(rows: &[Value], x_col: &str) -> XMode {
    let Some(x_raw) = rows
        .first()
        .and_then(Value::as_object)
        .and_then(|obj| obj.get(x_col))
    else {
        return XMode::Categorical;
    };
    if as_number(x_raw).is_some() {
        return XMode::Numeric;
    }
    if let Some(s) = x_raw.as_str() {
        if let Some((kind, _)) = parse_temporal(s) {
            return XMode::Temporal(kind);
        }
    }
    XMode::Categorical
}

/// Format a Unix epoch seconds tick value as a human-readable date
/// string in a form matching the originally detected [`TemporalKind`].
fn format_temporal_tick(seconds: f64, kind: TemporalKind) -> String {
    if !seconds.is_finite() {
        return String::new();
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "tick positions for typical chart ranges (1970..2100) fit comfortably in i64; pre-flight is_finite() guards NaN/inf, and timestamp_opt() returns None on out-of-range values which we map to empty string"
    )]
    let secs_i64 = seconds.round() as i64;
    match kind {
        TemporalKind::Date => Utc
            .timestamp_opt(secs_i64, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        TemporalKind::DateTime => Utc
            .timestamp_opt(secs_i64, 0)
            .single()
            .map(|dt| dt.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default(),
        TemporalKind::DateTimeTz(tz_offset) => FixedOffset::east_opt(tz_offset)
            .and_then(|off| off.timestamp_opt(secs_i64, 0).single())
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S%:z").to_string())
            .unwrap_or_default(),
    }
}

/// Group rows into (`series_name`, points) buckets, extracting x and y values.
/// When `series_col` is None, all points land in a single unnamed series.
fn group_series(
    rows: &[Value],
    x_col: &str,
    y_col: &str,
    series_col: Option<&str>,
    x_mode: XMode,
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
        let x_val = match x_mode {
            XMode::Categorical => {
                let next = category_index.len() as f64;
                *category_index.entry(x_label.clone()).or_insert(next)
            }
            XMode::Numeric => as_number(&x_raw).ok_or_else(|| {
                McpError::new(
                    ErrorCode::SchemaMismatch,
                    format!("Column '{x_col}' is missing or not numeric in at least one row"),
                )
            })?,
            XMode::Temporal(_) => parse_temporal(&x_label)
                .map(|(_, ts)| ts)
                .ok_or_else(|| {
                    McpError::new(
                        ErrorCode::SchemaMismatch,
                        format!(
                            "Column '{x_col}' value '{x_label}' is not a recognized DATE / TIMESTAMP / TIMESTAMPTZ form"
                        ),
                    )
                })?,
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

    // Bar charts default to categorical x axis; `ChartOptions::x_as_category=Some(false)`
    // lets callers force numeric if they really want to. Bar charts never
    // use temporal mode — even time-series bar charts visually expect
    // discrete bars at evenly-spaced positions.
    let x_mode = if opts.x_as_category == Some(false) {
        XMode::Numeric
    } else {
        XMode::Categorical
    };
    let groups = group_series(rows, x_col, y_col, opts.series_column.as_deref(), x_mode)?;

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
    let labels = strip_shared_tz_suffix(&raw_labels);
    let tick_count = auto_tick_count(&labels, opts.width);
    chart
        .configure_mesh()
        .x_labels(tick_count)
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
    // Decide the x mode:
    // - Explicit `x_as_category=Some(true)` → Categorical (force).
    // - Explicit `x_as_category=Some(false)` → Numeric (force).
    // - Default (None): peek at the first row's x value:
    //   - parses as DATE/TIMESTAMP/TIMESTAMPTZ → Temporal (proportional time axis).
    //   - non-numeric (TEXT) → Categorical fallback.
    //   - numeric → Numeric.
    let x_mode = match opts.x_as_category {
        Some(true) => XMode::Categorical,
        Some(false) => XMode::Numeric,
        None => detect_line_x_mode(rows, x_col),
    };
    let groups = group_series(rows, x_col, y_col, opts.series_column.as_deref(), x_mode)?;

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
        .x_label_area_size(match x_mode {
            XMode::Categorical | XMode::Temporal(_) => 60,
            XMode::Numeric => 50,
        })
        .y_label_area_size(70)
        .build_cartesian_2d(rx_min..rx_max, ry_min..ry_max)
        .map_err(draw_err)?;

    // Configure the x-axis ticks per mode:
    // - Categorical: tick positions are synthetic indices; the formatter
    //   maps each back to the original string label.
    // - Temporal: tick positions are real Unix epoch seconds (proportional
    //   to wall-clock time); the formatter renders each via chrono in a
    //   format matching the input kind (DATE / TIMESTAMP / TIMESTAMPTZ).
    // - Numeric: pass-through; plotters' default float formatter is fine.
    match x_mode {
        XMode::Categorical => {
            let categories = collect_categories(&groups);
            let raw_labels: Vec<String> = categories.iter().map(|(_, l)| l.clone()).collect();
            let labels = strip_shared_tz_suffix(&raw_labels);
            let tick_count = auto_tick_count(&labels, opts.width);
            chart
                .configure_mesh()
                .x_desc(x_col)
                .y_desc(y_col)
                .x_labels(tick_count)
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
        }
        XMode::Temporal(kind) => {
            // Sample one rendered tick label to size the per-tick budget.
            // DATE → 10 chars, TIMESTAMP → 19, TIMESTAMPTZ → 25 (with
            // `+HH:MM`). Floor at 10 so a degenerate sample still gets
            // a reasonable per-label budget.
            let sample = format_temporal_tick(rx_min, kind);
            let sample_chars = sample.chars().count().max(10);
            let tick_count = tick_count_for_label_width(sample_chars, opts.width);
            chart
                .configure_mesh()
                .x_desc(x_col)
                .y_desc(y_col)
                .x_labels(tick_count)
                .x_label_formatter(&|v| format_temporal_tick(*v, kind))
                .draw()
                .map_err(draw_err)?;
        }
        XMode::Numeric => {
            chart
                .configure_mesh()
                .x_desc(x_col)
                .y_desc(y_col)
                .draw()
                .map_err(draw_err)?;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn s(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn strip_shared_tz_suffix_drops_uniform_offset() {
        let labels = s(&[
            "2026-05-01 08:00:00+00:00",
            "2026-05-02 06:15:00+00:00",
            "2026-05-03 18:30:00+00:00",
        ]);
        let stripped = strip_shared_tz_suffix(&labels);
        assert_eq!(
            stripped,
            s(&[
                "2026-05-01 08:00:00",
                "2026-05-02 06:15:00",
                "2026-05-03 18:30:00",
            ])
        );
    }

    #[test]
    fn strip_shared_tz_suffix_handles_non_utc_offset() {
        let labels = s(&["2026-05-01 08:00:00+05:30", "2026-05-02 06:15:00+05:30"]);
        let stripped = strip_shared_tz_suffix(&labels);
        assert_eq!(
            stripped,
            s(&["2026-05-01 08:00:00", "2026-05-02 06:15:00",])
        );
    }

    #[test]
    fn strip_shared_tz_suffix_preserves_when_offsets_differ() {
        let labels = s(&["2026-05-01 08:00:00+00:00", "2026-05-02 06:15:00+05:30"]);
        let stripped = strip_shared_tz_suffix(&labels);
        assert_eq!(stripped, labels, "differing offsets must not be stripped");
    }

    #[test]
    fn strip_shared_tz_suffix_preserves_plain_dates() {
        let labels = s(&["2026-05-01", "2026-05-02", "2026-05-03"]);
        let stripped = strip_shared_tz_suffix(&labels);
        assert_eq!(stripped, labels, "DATE strings have no suffix to strip");
    }

    #[test]
    fn strip_shared_tz_suffix_passes_through_one_or_zero() {
        assert_eq!(strip_shared_tz_suffix(&[]), Vec::<String>::new());
        let one = s(&["2026-05-01 08:00:00+00:00"]);
        assert_eq!(strip_shared_tz_suffix(&one), one);
    }

    #[test]
    fn auto_tick_count_returns_all_when_labels_fit() {
        // 5 short labels at width 800 — all fit comfortably.
        let labels = s(&["A", "B", "C", "D", "E"]);
        assert_eq!(auto_tick_count(&labels, 800), 5);
    }

    #[test]
    fn auto_tick_count_thins_long_timestamp_series() {
        // 90 points like "2026-01-01 13:00:00" (19 chars).
        // per_label_px = 19*7 + 10 = 143; fits = 800/143 = 5.
        // The fix's contract: the count plotters is told MUST be ≥ 2
        // (so the axis stays informative) and ≤ labels.len(); for a
        // 19-char label at 800px the heuristic should land in the
        // 4..=8 band — comfortably small enough that no two adjacent
        // ticks overlap.
        let labels: Vec<String> = (0..90)
            .map(|i| format!("2026-01-{:02} {:02}:00:00", (i / 24) + 1, i % 24))
            .collect();
        let count = auto_tick_count(&labels, 800);
        assert!(
            (4..=8).contains(&count),
            "expected 4..=8 ticks for 90 long labels at 800px, got {count}"
        );
        assert!(count >= 2, "must always show at least 2 ticks");
        assert!(count <= labels.len(), "must never exceed label count");
    }

    #[test]
    fn auto_tick_count_clamps_to_at_least_two() {
        // Hypothetical: extremely wide labels at narrow chart width.
        let labels = s(&[
            "x".repeat(200).as_str(),
            "y".repeat(200).as_str(),
            "z".repeat(200).as_str(),
        ]);
        assert!(auto_tick_count(&labels, 100) >= 2);
    }

    #[test]
    fn auto_tick_count_handles_one_or_zero_labels() {
        assert_eq!(auto_tick_count(&[], 800), 0);
        let one = s(&["only"]);
        assert_eq!(auto_tick_count(&one, 800), 1);
    }

    #[test]
    fn auto_tick_count_caps_at_label_count() {
        // Tiny labels at huge width — heuristic would say "many", but
        // we should never exceed the actual label count.
        let labels = s(&["A", "B", "C"]);
        assert_eq!(auto_tick_count(&labels, 10_000), 3);
    }

    #[test]
    fn tick_count_for_label_width_does_not_clamp_to_label_count() {
        // The width-only helper has no label-count input, so a 19-char
        // estimate at 800px must compute fits=5 directly. Regression
        // guard against the bug where an over-eager `min(labels.len())`
        // collapsed the temporal-mode tick budget to 2.
        // 19 chars * 7 + 10 = 143px → 800/143 = 5, 1400/143 = 9.
        assert_eq!(tick_count_for_label_width(19, 800), 5);
        assert_eq!(tick_count_for_label_width(19, 1400), 9);
        // 10 chars * 7 + 10 = 80px → 800/80 = 10. DATE-only fits more.
        assert_eq!(tick_count_for_label_width(10, 800), 10);
    }

    #[test]
    fn tick_count_for_label_width_clamps_to_at_least_two() {
        assert_eq!(tick_count_for_label_width(200, 100), 2);
    }

    #[test]
    fn parse_temporal_recognizes_date() {
        let (kind, secs) = parse_temporal("2026-05-01").expect("DATE should parse");
        assert_eq!(kind, TemporalKind::Date);
        // Sanity: well after the epoch.
        assert!(secs > 1.7e9);
    }

    #[test]
    fn parse_temporal_recognizes_timestamp() {
        let (kind, secs1) = parse_temporal("2026-05-01 08:00:00").expect("TIMESTAMP should parse");
        assert_eq!(kind, TemporalKind::DateTime);
        let (_, secs2) = parse_temporal("2026-05-01 12:30:00").expect("TIMESTAMP should parse");
        // Same date, 4.5 hours apart.
        let delta = secs2 - secs1;
        assert!(
            (delta - 16_200.0).abs() < 1.0,
            "expected 16200s gap, got {delta}"
        );
    }

    #[test]
    fn parse_temporal_recognizes_timestamptz_and_captures_offset() {
        let (kind, _) =
            parse_temporal("2026-05-01 08:00:00+05:30").expect("TIMESTAMPTZ should parse");
        match kind {
            TemporalKind::DateTimeTz(off) => assert_eq!(off, 5 * 3600 + 30 * 60),
            other => panic!("expected DateTimeTz, got {other:?}"),
        }
    }

    #[test]
    fn parse_temporal_recognizes_t_separator() {
        let (kind, _) =
            parse_temporal("2026-05-01T08:00:00+00:00").expect("ISO T-form should parse");
        assert!(matches!(kind, TemporalKind::DateTimeTz(0)));
    }

    #[test]
    fn parse_temporal_rejects_non_temporal_strings() {
        assert!(parse_temporal("alpha").is_none());
        assert!(parse_temporal("").is_none());
        assert!(parse_temporal("2026").is_none());
        // Numeric strings are NOT temporal — caller should treat as numeric.
        assert!(parse_temporal("42").is_none());
    }

    #[test]
    fn format_temporal_tick_round_trips_date() {
        let (_, secs) = parse_temporal("2026-05-01").unwrap();
        assert_eq!(format_temporal_tick(secs, TemporalKind::Date), "2026-05-01");
    }

    #[test]
    fn format_temporal_tick_round_trips_timestamp() {
        let (_, secs) = parse_temporal("2026-05-01 08:30:00").unwrap();
        assert_eq!(
            format_temporal_tick(secs, TemporalKind::DateTime),
            "2026-05-01 08:30:00"
        );
    }

    #[test]
    fn format_temporal_tick_preserves_offset_for_timestamptz() {
        let (kind, secs) = parse_temporal("2026-05-01 08:30:00+05:30").unwrap();
        assert_eq!(
            format_temporal_tick(secs, kind),
            "2026-05-01 08:30:00+05:30"
        );
    }

    #[test]
    fn format_temporal_tick_handles_nan() {
        // Plotters can theoretically pass NaN/infinity for axis ticks
        // when the range is degenerate. We must not panic.
        assert_eq!(format_temporal_tick(f64::NAN, TemporalKind::Date), "");
        assert_eq!(
            format_temporal_tick(f64::INFINITY, TemporalKind::DateTime),
            ""
        );
    }

    #[test]
    fn detect_line_x_mode_picks_temporal_for_dates() {
        let rows = vec![serde_json::json!({"ts": "2026-05-01"})];
        let mode = detect_line_x_mode(&rows, "ts");
        assert!(matches!(mode, XMode::Temporal(TemporalKind::Date)));
    }

    #[test]
    fn detect_line_x_mode_picks_temporal_for_timestamps() {
        let rows = vec![serde_json::json!({"ts": "2026-05-01 08:00:00"})];
        let mode = detect_line_x_mode(&rows, "ts");
        assert!(matches!(mode, XMode::Temporal(TemporalKind::DateTime)));
    }

    #[test]
    fn detect_line_x_mode_picks_temporal_for_timestamptz() {
        let rows = vec![serde_json::json!({"ts": "2026-05-01 08:00:00+00:00"})];
        let mode = detect_line_x_mode(&rows, "ts");
        assert!(matches!(mode, XMode::Temporal(TemporalKind::DateTimeTz(0))));
    }

    #[test]
    fn detect_line_x_mode_falls_back_to_categorical_for_text() {
        let rows = vec![serde_json::json!({"x": "alpha"})];
        let mode = detect_line_x_mode(&rows, "x");
        assert!(matches!(mode, XMode::Categorical));
    }

    #[test]
    fn detect_line_x_mode_picks_numeric_for_numbers() {
        let rows = vec![serde_json::json!({"x": 42.0})];
        let mode = detect_line_x_mode(&rows, "x");
        assert!(matches!(mode, XMode::Numeric));
    }
}
