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

use crate::engine::ChartMeasureValue;
use crate::error::{ErrorCode, McpError};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use plotters::coord::ranged1d::ValueFormatter;
use plotters::coord::types::RangedCoordf64;
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
#[cfg(test)]
type SeriesPoints = Vec<(f64, f64, String)>;

/// Series name → its points. Uses `BTreeMap` (not `HashMap`) so
/// multi-series charts render in deterministic order, which makes
/// the resulting image bytes reproducible across runs.
#[cfg(test)]
type SeriesMap = BTreeMap<String, SeriesPoints>;

/// Renderer point retaining both numeric coordinates and the caller-visible
/// scalar text. The latter is required for bar value labels: formatting the
/// converted `f64` would lose the exact representation returned by SQL.
#[derive(Debug, Clone)]
struct ChartPoint {
    x: f64,
    y: f64,
    x_label: String,
    y_label: String,
}

type ChartSeriesMap = BTreeMap<String, Vec<ChartPoint>>;

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
    /// - `Some(false)`: force numeric x for `Line` / `Scatter`. Bar charts
    ///   remain categorical regardless of this setting.
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
    /// Fix the data-role y measure range as `[min, max]`. Unlike `x_range`,
    /// this applies to bar charts; horizontal bars render it on the physical
    /// x axis. Log ranges must be positive and contain every plotted value.
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

/// Physical layout for bar marks. This remains crate-private so the public
/// Rust renderer keeps its legacy [`ChartOptions`] surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarOrientation {
    Vertical,
    Horizontal,
}

/// Scale applied to the data-role y measure. Horizontal bars still use this
/// as their physical x scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MeasureScale {
    Linear,
    Log,
}

/// MCP-only presentation controls for the private extended renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChartPresentation {
    bar_orientation: BarOrientation,
    label_values: bool,
    show_legend: bool,
    y_scale: MeasureScale,
}

impl Default for ChartPresentation {
    fn default() -> Self {
        Self {
            bar_orientation: BarOrientation::Vertical,
            label_values: false,
            show_legend: true,
            y_scale: MeasureScale::Linear,
        }
    }
}

impl ChartPresentation {
    /// Parse MCP string controls without placing serde enums at the transport
    /// boundary. Unknown values therefore become the same structured
    /// `INVALID_ARGUMENT` tool errors as invalid cross-chart combinations.
    pub(crate) fn from_mcp(
        chart_type: ChartType,
        bar_orientation: Option<&str>,
        label_values: Option<bool>,
        show_legend: Option<bool>,
        y_scale: Option<&str>,
    ) -> Result<Self, McpError> {
        if bar_orientation.is_some() && chart_type != ChartType::Bar {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                "bar_orientation is only valid for bar charts",
            ));
        }
        if label_values == Some(true) && chart_type != ChartType::Bar {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                "label_values=true is only valid for bar charts",
            ));
        }

        let bar_orientation = match bar_orientation {
            None => BarOrientation::Vertical,
            Some(value) if value.eq_ignore_ascii_case("vertical") => BarOrientation::Vertical,
            Some(value) if value.eq_ignore_ascii_case("horizontal") => BarOrientation::Horizontal,
            Some(value) => {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "Unknown bar_orientation '{value}'. Expected 'vertical' or 'horizontal'"
                    ),
                ));
            }
        };
        let y_scale = match y_scale {
            None => MeasureScale::Linear,
            Some(value) if value.eq_ignore_ascii_case("linear") => MeasureScale::Linear,
            Some(value) if value.eq_ignore_ascii_case("log") => MeasureScale::Log,
            Some(value) => {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!("Unknown y_scale '{value}'. Expected 'linear' or 'log'"),
                ));
            }
        };

        let presentation = Self {
            bar_orientation,
            label_values: label_values.unwrap_or(false),
            show_legend: show_legend.unwrap_or(true),
            y_scale,
        };
        presentation.validate(chart_type)?;
        Ok(presentation)
    }

    fn validate(self, chart_type: ChartType) -> Result<(), McpError> {
        if self.label_values && chart_type != ChartType::Bar {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                "label_values=true is only valid for bar charts",
            ));
        }
        if self.bar_orientation == BarOrientation::Horizontal && chart_type != ChartType::Bar {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                "horizontal bar_orientation is only valid for bar charts",
            ));
        }
        if self.y_scale == MeasureScale::Log && chart_type == ChartType::Histogram {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                "y_scale=log is not supported for histograms",
            ));
        }
        Ok(())
    }
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
///   50,000 rows or an explicit range is non-finite, non-increasing, or lacks
///   a finite representable span in the selected coordinate system.
pub fn render_chart(rows: &[Value], opts: &ChartOptions) -> Result<ChartResult, McpError> {
    render_chart_with_presentation(rows, opts, ChartPresentation::default())
}

/// Extended renderer used by the MCP chart tool while the public Rust API
/// remains source-compatible.
pub(crate) fn render_chart_with_presentation(
    rows: &[Value],
    opts: &ChartOptions,
    presentation: ChartPresentation,
) -> Result<ChartResult, McpError> {
    render_chart_impl(rows, opts, presentation, None)
}

/// Extended MCP renderer that consumes row-aligned typed measure metadata.
/// The public JSON renderer delegates without this sidecar and retains its
/// established source and behavior contract.
pub(crate) fn render_chart_with_measure_metadata(
    rows: &[Value],
    opts: &ChartOptions,
    presentation: ChartPresentation,
    measures: &[ChartMeasureValue],
) -> Result<ChartResult, McpError> {
    render_chart_impl(rows, opts, presentation, Some(measures))
}

fn render_chart_impl(
    rows: &[Value],
    opts: &ChartOptions,
    presentation: ChartPresentation,
    measures: Option<&[ChartMeasureValue]>,
) -> Result<ChartResult, McpError> {
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
    if measures.is_some_and(|values| values.len() != rows.len()) {
        return Err(McpError::new(
            ErrorCode::InternalError,
            "Chart measure metadata is not aligned with the query rows",
        ));
    }

    validate_explicit_range("x_range", opts.x_range)?;
    validate_explicit_range("y_range", opts.y_range)?;
    presentation.validate(opts.chart_type)?;

    match opts.format {
        ChartFormat::Png => render_png(rows, opts, presentation, measures),
        ChartFormat::Svg => render_svg(rows, opts, presentation, measures),
    }
}

fn validate_explicit_range(name: &str, range: Option<[f64; 2]>) -> Result<(), McpError> {
    let Some([lo, hi]) = range else {
        return Ok(());
    };
    validate_effective_linear_range(name, (lo, hi)).map(|_| ())
}

fn validate_effective_linear_range(
    name: &str,
    (lo, hi): (f64, f64),
) -> Result<(f64, f64), McpError> {
    if !lo.is_finite() || !hi.is_finite() || lo >= hi || !(hi - lo).is_finite() {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!(
                "{name} must contain two finite values in strictly increasing order with a finite span"
            ),
        ));
    }
    Ok((lo, hi))
}

fn render_png(
    rows: &[Value],
    opts: &ChartOptions,
    presentation: ChartPresentation,
    measures: Option<&[ChartMeasureValue]>,
) -> Result<ChartResult, McpError> {
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
        draw_on_backend(backend, rows, opts, presentation, measures)?
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

fn render_svg(
    rows: &[Value],
    opts: &ChartOptions,
    presentation: ChartPresentation,
    measures: Option<&[ChartMeasureValue]>,
) -> Result<ChartResult, McpError> {
    let mut svg_string = String::new();
    let rows_plotted = {
        let backend = SVGBackend::with_string(&mut svg_string, (opts.width, opts.height));
        draw_on_backend(backend, rows, opts, presentation, measures)?
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
    presentation: ChartPresentation,
    measures: Option<&[ChartMeasureValue]>,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let root = backend.into_drawing_area();
    root.fill(&WHITE).map_err(draw_err)?;

    match opts.chart_type {
        ChartType::Bar => draw_bar(&root, rows, opts, presentation, measures),
        ChartType::Line => draw_line(&root, rows, opts, presentation, measures),
        ChartType::Scatter => draw_scatter(&root, rows, opts, presentation, measures),
        ChartType::Histogram => draw_histogram(&root, rows, opts, measures),
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

/// Bound horizontal categorical tick labels by the vertical pixels available
/// to the plot. The fixed deduction covers the caption, margins, and physical
/// x-axis label area; the remaining height uses a conservative twelve-pixel
/// pitch for the mesh font. A single category always keeps its one label.
fn horizontal_category_tick_count(category_count: usize, chart_height: u32) -> usize {
    const NON_PLOT_HEIGHT_PX: u32 = 100;
    const MIN_LABEL_PITCH_PX: u32 = 12;

    if category_count <= 1 {
        return category_count;
    }
    let available_height = chart_height.saturating_sub(NON_PLOT_HEIGHT_PX);
    let fits = usize::try_from(available_height / MIN_LABEL_PITCH_PX)
        .unwrap_or(usize::MAX)
        .max(2);
    fits.min(category_count)
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
#[cfg(test)]
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

fn collect_chart_categories(groups: &ChartSeriesMap) -> Vec<(f64, String)> {
    let mut seen: BTreeMap<u64, String> = BTreeMap::new();
    for points in groups.values() {
        for point in points {
            seen.entry(point.x.to_bits())
                .or_insert_with(|| point.x_label.clone());
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
#[cfg(test)]
fn group_series(
    rows: &[Value],
    x_col: &str,
    y_col: &str,
    series_col: Option<&str>,
    x_mode: XMode,
) -> Result<SeriesMap, McpError> {
    group_chart_series(rows, x_col, y_col, series_col, x_mode, None).map(|groups| {
        groups
            .into_iter()
            .map(|(series, points)| {
                let points = points
                    .into_iter()
                    .map(|point| (point.x, point.y, point.x_label))
                    .collect();
                (series, points)
            })
            .collect()
    })
}

fn group_chart_series(
    rows: &[Value],
    x_col: &str,
    y_col: &str,
    series_col: Option<&str>,
    x_mode: XMode,
    measures: Option<&[ChartMeasureValue]>,
) -> Result<ChartSeriesMap, McpError> {
    let mut groups: ChartSeriesMap = BTreeMap::new();
    let mut category_index: BTreeMap<String, f64> = BTreeMap::new();

    for (row_index, row) in rows.iter().enumerate() {
        let Some(obj) = row.as_object() else { continue };

        let y_raw = obj.get(y_col).ok_or_else(|| {
            McpError::new(
                ErrorCode::SchemaMismatch,
                format!("Column '{y_col}' is missing or not numeric in at least one row"),
            )
        })?;
        let (y_val, y_label) = chart_measure_coordinate_and_label(
            measures.and_then(|values| values.get(row_index)),
            y_raw,
            y_col,
        )?;

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

        groups.entry(series_key).or_default().push(ChartPoint {
            x: x_val,
            y: y_val,
            x_label,
            y_label,
        });
    }

    if groups.values().all(std::vec::Vec::is_empty) {
        return Err(McpError::new(
            ErrorCode::EmptyData,
            "No valid data points after filtering",
        ));
    }

    Ok(groups)
}

fn chart_measure_coordinate_and_label(
    measure: Option<&ChartMeasureValue>,
    json_value: &Value,
    column: &str,
) -> Result<(f64, String), McpError> {
    match measure {
        Some(ChartMeasureValue::Finite {
            coordinate,
            display,
        }) => Ok((*coordinate, display.clone())),
        Some(ChartMeasureValue::NonFinite) => Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!("Column '{column}' contains a non-finite numeric value"),
        )),
        Some(ChartMeasureValue::Null | ChartMeasureValue::NonNumeric) => {
            Err(non_numeric_measure_error(column))
        }
        None => as_number(json_value)
            .map(|coordinate| (coordinate, as_string(json_value)))
            .ok_or_else(|| non_numeric_measure_error(column)),
    }
}

fn non_numeric_measure_error(column: &str) -> McpError {
    McpError::new(
        ErrorCode::SchemaMismatch,
        format!("Column '{column}' is missing or not numeric in at least one row"),
    )
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
    // Guard `is_ascii()` before the byte slices below: a 6-*byte* multi-byte
    // string (e.g. "1é234", where `é` is two bytes) passes `len() != 6` but
    // `&s[0..2]` would land mid-codepoint and panic. ASCII guarantees one
    // byte per char, so every `[0..2]`/`[2..4]`/`[4..6]` is a char boundary.
    if !s.is_ascii() || s.len() != 6 {
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
    presentation: ChartPresentation,
    measures: Option<&[ChartMeasureValue]>,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let x_col = require_column(&opts.x_column, "x")?;
    let y_col = require_column(&opts.y_column, "y")?;
    // A bar's x value is always a category. In particular, a numeric JSON
    // scalar must not be interpreted as a physical x coordinate when the
    // legacy `x_as_category:false` flag is present.
    let groups = group_chart_series(
        rows,
        x_col,
        y_col,
        opts.series_column.as_deref(),
        XMode::Categorical,
        measures,
    )?;
    let categories = collect_chart_categories(&groups);
    let values: Vec<f64> = groups
        .values()
        .flat_map(|points| points.iter().map(|point| point.y))
        .collect();
    let measure_range = match presentation.y_scale {
        MeasureScale::Linear => linear_bar_range(&values, opts.y_range)?,
        MeasureScale::Log => log_measure_range(&values, opts.y_range)?,
    };
    let title = opts
        .title
        .clone()
        .unwrap_or_else(|| format!("{y_col} by {x_col}"));

    match (presentation.bar_orientation, presentation.y_scale) {
        (BarOrientation::Vertical, MeasureScale::Linear) => draw_vertical_bar_linear(
            root,
            &groups,
            &categories,
            opts,
            x_col,
            y_col,
            &title,
            measure_range,
            presentation,
        ),
        (BarOrientation::Vertical, MeasureScale::Log) => draw_vertical_bar_log(
            root,
            &groups,
            &categories,
            opts,
            x_col,
            y_col,
            &title,
            measure_range,
            presentation,
        ),
        (BarOrientation::Horizontal, MeasureScale::Linear) => draw_horizontal_bar_linear(
            root,
            &groups,
            &categories,
            opts,
            x_col,
            y_col,
            &title,
            measure_range,
            presentation,
        ),
        (BarOrientation::Horizontal, MeasureScale::Log) => draw_horizontal_bar_log(
            root,
            &groups,
            &categories,
            opts,
            x_col,
            y_col,
            &title,
            measure_range,
            presentation,
        ),
    }
}

fn linear_bar_range(values: &[f64], explicit: Option<[f64; 2]>) -> Result<(f64, f64), McpError> {
    if let Some([lo, hi]) = explicit {
        return validate_effective_linear_range("y_range", (lo, hi));
    }
    let lo = values
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min)
        .min(0.0);
    let hi = values
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max)
        .max(0.0);
    let pad = (hi - lo).abs() * 0.1 + 1.0;
    validate_effective_linear_range("derived bar y-axis range", (lo - pad, hi + pad))
}

fn linear_bar_baseline((lo, hi): (f64, f64)) -> f64 {
    if lo <= 0.0 && hi >= 0.0 {
        0.0
    } else if lo > 0.0 {
        lo
    } else {
        hi
    }
}

fn draw_vertical_bar_linear<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    groups: &ChartSeriesMap,
    categories: &[(f64, String)],
    opts: &ChartOptions,
    x_col: &str,
    y_col: &str,
    title: &str,
    measure_range: (f64, f64),
    presentation: ChartPresentation,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let category_range = -0.5_f64..categories.len() as f64 - 0.5;
    let mut chart = ChartBuilder::on(root)
        .caption(title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(60)
        .y_label_area_size(70)
        .build_cartesian_2d(category_range, measure_range.0..measure_range.1)
        .map_err(draw_err)?;
    configure_vertical_bar_mesh(&mut chart, categories, opts.width, x_col, y_col)?;

    let baseline = linear_bar_baseline(measure_range);
    let num_series = groups.len().max(1);
    let total_width = 0.8_f64;
    let bar_width = total_width / num_series as f64;
    let label_style = ("sans-serif", 11).into_font().color(&BLACK);
    let mut total_plotted = 0usize;
    for (idx, (series_key, points)) in groups.iter().enumerate() {
        let color = series_color_for(series_key, idx, opts);
        let offset = -total_width / 2.0 + bar_width * (idx as f64 + 0.5);
        let name = bar_series_name(series_key, y_col);
        let annotation = chart
            .draw_series(points.iter().map(|point| {
                let left = point.x + offset - bar_width / 2.0;
                let right = point.x + offset + bar_width / 2.0;
                Rectangle::new([(left, baseline), (right, point.y)], color.filled())
            }))
            .map_err(draw_err)?;
        if presentation.show_legend {
            annotation.label(name).legend(move |(x, y)| {
                Rectangle::new([(x, y - 5), (x + 12, y + 5)], color.filled())
            });
        }
        if presentation.label_values {
            chart
                .draw_series(points.iter().map(|point| {
                    EmptyElement::at((point.x + offset, point.y))
                        + Text::new(point.y_label.clone(), (0, -5), label_style.clone())
                }))
                .map_err(draw_err)?;
        }
        total_plotted += points.len();
    }
    if presentation.show_legend {
        draw_series_legend(&mut chart)?;
    }
    root.present().map_err(draw_err)?;
    Ok(total_plotted)
}

fn draw_vertical_bar_log<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    groups: &ChartSeriesMap,
    categories: &[(f64, String)],
    opts: &ChartOptions,
    x_col: &str,
    y_col: &str,
    title: &str,
    measure_range: (f64, f64),
    presentation: ChartPresentation,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let category_range = -0.5_f64..categories.len() as f64 - 0.5;
    let mut chart = ChartBuilder::on(root)
        .caption(title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(60)
        .y_label_area_size(70)
        .build_cartesian_2d(
            category_range,
            (measure_range.0..measure_range.1)
                .log_scale()
                .with_key_points(bounded_log_key_points(measure_range)),
        )
        .map_err(draw_err)?;
    configure_vertical_bar_mesh(&mut chart, categories, opts.width, x_col, y_col)?;

    let baseline = measure_range.0;
    let num_series = groups.len().max(1);
    let total_width = 0.8_f64;
    let bar_width = total_width / num_series as f64;
    let label_style = ("sans-serif", 11).into_font().color(&BLACK);
    let mut total_plotted = 0usize;
    for (idx, (series_key, points)) in groups.iter().enumerate() {
        let color = series_color_for(series_key, idx, opts);
        let offset = -total_width / 2.0 + bar_width * (idx as f64 + 0.5);
        let name = bar_series_name(series_key, y_col);
        let annotation = chart
            .draw_series(points.iter().map(|point| {
                let left = point.x + offset - bar_width / 2.0;
                let right = point.x + offset + bar_width / 2.0;
                Rectangle::new([(left, baseline), (right, point.y)], color.filled())
            }))
            .map_err(draw_err)?;
        if presentation.show_legend {
            annotation.label(name).legend(move |(x, y)| {
                Rectangle::new([(x, y - 5), (x + 12, y + 5)], color.filled())
            });
        }
        if presentation.label_values {
            chart
                .draw_series(points.iter().map(|point| {
                    EmptyElement::at((point.x + offset, point.y))
                        + Text::new(point.y_label.clone(), (0, -5), label_style.clone())
                }))
                .map_err(draw_err)?;
        }
        total_plotted += points.len();
    }
    if presentation.show_legend {
        draw_series_legend(&mut chart)?;
    }
    root.present().map_err(draw_err)?;
    Ok(total_plotted)
}

fn draw_horizontal_bar_linear<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    groups: &ChartSeriesMap,
    categories: &[(f64, String)],
    opts: &ChartOptions,
    x_col: &str,
    y_col: &str,
    title: &str,
    measure_range: (f64, f64),
    presentation: ChartPresentation,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let category_range = categories.len() as f64 - 0.5..-0.5_f64;
    let mut chart = ChartBuilder::on(root)
        .caption(title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(70)
        .y_label_area_size(160)
        .build_cartesian_2d(measure_range.0..measure_range.1, category_range)
        .map_err(draw_err)?;
    configure_horizontal_bar_mesh(&mut chart, categories, opts.height, x_col, y_col)?;

    let baseline = linear_bar_baseline(measure_range);
    let num_series = groups.len().max(1);
    let total_width = 0.8_f64;
    let bar_width = total_width / num_series as f64;
    let label_style = ("sans-serif", 11).into_font().color(&BLACK);
    let mut total_plotted = 0usize;
    for (idx, (series_key, points)) in groups.iter().enumerate() {
        let color = series_color_for(series_key, idx, opts);
        let offset = -total_width / 2.0 + bar_width * (idx as f64 + 0.5);
        let name = bar_series_name(series_key, y_col);
        let annotation = chart
            .draw_series(points.iter().map(|point| {
                let top = point.x + offset - bar_width / 2.0;
                let bottom = point.x + offset + bar_width / 2.0;
                Rectangle::new([(baseline, top), (point.y, bottom)], color.filled())
            }))
            .map_err(draw_err)?;
        if presentation.show_legend {
            annotation.label(name).legend(move |(x, y)| {
                Rectangle::new([(x, y - 5), (x + 12, y + 5)], color.filled())
            });
        }
        if presentation.label_values {
            chart
                .draw_series(points.iter().map(|point| {
                    EmptyElement::at((point.y, point.x + offset))
                        + Text::new(point.y_label.clone(), (5, 0), label_style.clone())
                }))
                .map_err(draw_err)?;
        }
        total_plotted += points.len();
    }
    if presentation.show_legend {
        draw_series_legend(&mut chart)?;
    }
    root.present().map_err(draw_err)?;
    Ok(total_plotted)
}

fn draw_horizontal_bar_log<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    groups: &ChartSeriesMap,
    categories: &[(f64, String)],
    opts: &ChartOptions,
    x_col: &str,
    y_col: &str,
    title: &str,
    measure_range: (f64, f64),
    presentation: ChartPresentation,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let category_range = categories.len() as f64 - 0.5..-0.5_f64;
    let mut chart = ChartBuilder::on(root)
        .caption(title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(70)
        .y_label_area_size(160)
        .build_cartesian_2d(
            (measure_range.0..measure_range.1)
                .log_scale()
                .with_key_points(bounded_log_key_points(measure_range)),
            category_range,
        )
        .map_err(draw_err)?;
    configure_horizontal_bar_mesh(&mut chart, categories, opts.height, x_col, y_col)?;

    let baseline = measure_range.0;
    let num_series = groups.len().max(1);
    let total_width = 0.8_f64;
    let bar_width = total_width / num_series as f64;
    let label_style = ("sans-serif", 11).into_font().color(&BLACK);
    let mut total_plotted = 0usize;
    for (idx, (series_key, points)) in groups.iter().enumerate() {
        let color = series_color_for(series_key, idx, opts);
        let offset = -total_width / 2.0 + bar_width * (idx as f64 + 0.5);
        let name = bar_series_name(series_key, y_col);
        let annotation = chart
            .draw_series(points.iter().map(|point| {
                let top = point.x + offset - bar_width / 2.0;
                let bottom = point.x + offset + bar_width / 2.0;
                Rectangle::new([(baseline, top), (point.y, bottom)], color.filled())
            }))
            .map_err(draw_err)?;
        if presentation.show_legend {
            annotation.label(name).legend(move |(x, y)| {
                Rectangle::new([(x, y - 5), (x + 12, y + 5)], color.filled())
            });
        }
        if presentation.label_values {
            chart
                .draw_series(points.iter().map(|point| {
                    EmptyElement::at((point.y, point.x + offset))
                        + Text::new(point.y_label.clone(), (5, 0), label_style.clone())
                }))
                .map_err(draw_err)?;
        }
        total_plotted += points.len();
    }
    if presentation.show_legend {
        draw_series_legend(&mut chart)?;
    }
    root.present().map_err(draw_err)?;
    Ok(total_plotted)
}

fn configure_vertical_bar_mesh<DB, Y>(
    chart: &mut ChartContext<'_, DB, Cartesian2d<RangedCoordf64, Y>>,
    categories: &[(f64, String)],
    chart_width: u32,
    x_col: &str,
    y_col: &str,
) -> Result<(), McpError>
where
    DB: DrawingBackend,
    DB::ErrorType: 'static,
    Y: Ranged<ValueType = f64> + ValueFormatter<f64>,
{
    let raw_labels: Vec<String> = categories.iter().map(|(_, label)| label.clone()).collect();
    let labels = strip_shared_tz_suffix(&raw_labels);
    let label_map = category_label_map(categories, &labels);
    let tick_count = auto_tick_count(&labels, chart_width);
    chart
        .configure_mesh()
        .x_labels(tick_count)
        .x_label_formatter(&|value| category_label(*value, &label_map))
        .y_desc(y_col)
        .x_desc(x_col)
        .draw()
        .map_err(draw_err)
}

fn configure_horizontal_bar_mesh<DB, X>(
    chart: &mut ChartContext<'_, DB, Cartesian2d<X, RangedCoordf64>>,
    categories: &[(f64, String)],
    chart_height: u32,
    x_col: &str,
    y_col: &str,
) -> Result<(), McpError>
where
    DB: DrawingBackend,
    DB::ErrorType: 'static,
    X: Ranged<ValueType = f64> + ValueFormatter<f64>,
{
    let raw_labels: Vec<String> = categories.iter().map(|(_, label)| label.clone()).collect();
    let labels = strip_shared_tz_suffix(&raw_labels);
    let label_map = category_label_map(categories, &labels);
    let tick_count = horizontal_category_tick_count(labels.len(), chart_height);
    chart
        .configure_mesh()
        .y_labels(tick_count)
        .y_label_formatter(&|value| category_label(*value, &label_map))
        .x_desc(y_col)
        .y_desc(x_col)
        .draw()
        .map_err(draw_err)
}

fn category_label_map(categories: &[(f64, String)], labels: &[String]) -> BTreeMap<u64, String> {
    categories
        .iter()
        .zip(labels)
        .map(|((position, _), label)| (position.to_bits(), label.clone()))
        .collect()
}

fn category_label(value: f64, labels: &BTreeMap<u64, String>) -> String {
    value
        .is_finite()
        .then(|| value.round().to_bits())
        .and_then(|position| labels.get(&position).cloned())
        .unwrap_or_default()
}

fn bar_series_name(series_key: &str, y_col: &str) -> String {
    if series_key.is_empty() {
        y_col.to_string()
    } else {
        series_key.to_string()
    }
}

fn draw_series_legend<'a, DB, CT>(chart: &mut ChartContext<'a, DB, CT>) -> Result<(), McpError>
where
    DB: DrawingBackend + 'a,
    DB::ErrorType: 'static,
    CT: CoordTranslate,
{
    chart
        .configure_series_labels()
        .background_style(colors::WHITE.mix(0.9))
        .border_style(colors::BLACK)
        .draw()
        .map_err(draw_err)
}

fn bounded_log_key_points((lo, hi): (f64, f64)) -> Vec<f64> {
    const TICK_COUNT: usize = 7;
    const LAST_TICK: usize = TICK_COUNT - 1;

    let log_lo = lo.ln();
    let log_span = hi.ln() - log_lo;
    let denominator = LAST_TICK as f64;
    let mut ticks = Vec::with_capacity(TICK_COUNT);
    for index in 0..TICK_COUNT {
        let tick = if index == 0 {
            lo
        } else if index == LAST_TICK {
            hi
        } else {
            (log_lo + log_span * (index as f64 / denominator)).exp()
        };
        let follows_previous = match ticks.last() {
            Some(previous) => tick > *previous,
            None => true,
        };
        if tick.is_finite() && tick > 0.0 && tick >= lo && tick <= hi && follows_previous {
            ticks.push(tick);
        }
    }
    ticks
}

fn draw_line<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    rows: &[Value],
    opts: &ChartOptions,
    presentation: ChartPresentation,
    measures: Option<&[ChartMeasureValue]>,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    line_or_scatter(root, rows, opts, true, presentation, measures)
}

fn draw_scatter<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    rows: &[Value],
    opts: &ChartOptions,
    presentation: ChartPresentation,
    measures: Option<&[ChartMeasureValue]>,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    line_or_scatter(root, rows, opts, false, presentation, measures)
}

/// Shared implementation for line and scatter charts. `connect_points` controls
/// whether successive points are joined with a line.
fn line_or_scatter<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    rows: &[Value],
    opts: &ChartOptions,
    connect_points: bool,
    presentation: ChartPresentation,
    measures: Option<&[ChartMeasureValue]>,
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
    let groups = group_chart_series(
        rows,
        x_col,
        y_col,
        opts.series_column.as_deref(),
        x_mode,
        measures,
    )?;

    let default_title = if connect_points {
        "Line chart"
    } else {
        "Scatter plot"
    };
    let title = opts.title.clone().unwrap_or_else(|| default_title.into());

    match presentation.y_scale {
        MeasureScale::Linear => draw_line_or_scatter_linear(
            root,
            &groups,
            opts,
            x_col,
            y_col,
            x_mode,
            &title,
            connect_points,
            presentation,
        ),
        MeasureScale::Log => draw_line_or_scatter_log(
            root,
            &groups,
            opts,
            x_col,
            y_col,
            x_mode,
            &title,
            connect_points,
            presentation,
        ),
    }
}

fn draw_line_or_scatter_linear<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    groups: &ChartSeriesMap,
    opts: &ChartOptions,
    x_col: &str,
    y_col: &str,
    x_mode: XMode,
    title: &str,
    connect_points: bool,
    presentation: ChartPresentation,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let auto = bounds(groups);
    let (x_start, x_end, measure_floor, measure_ceiling) = apply_ranges(auto, opts)?;

    let mut chart = ChartBuilder::on(root)
        .caption(title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(match x_mode {
            XMode::Categorical | XMode::Temporal(_) => 60,
            XMode::Numeric => 50,
        })
        .y_label_area_size(70)
        .build_cartesian_2d(x_start..x_end, measure_floor..measure_ceiling)
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
            let categories = collect_chart_categories(groups);
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
            let sample = format_temporal_tick(x_start, kind);
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
    for (idx, (series_key, points)) in groups.iter().enumerate() {
        let color = series_color_for(series_key, idx, opts);
        let name = if series_key.is_empty() {
            y_col.to_string()
        } else {
            series_key.clone()
        };
        let mut sorted = points.clone();
        if connect_points {
            sorted.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        }

        if opts.label_points {
            // Draw dots/lines without registering a legend entry, then
            // annotate each point with the series name as a text label.
            if connect_points {
                chart
                    .draw_series(LineSeries::new(
                        sorted.iter().map(|point| (point.x, point.y)),
                        color.stroke_width(2),
                    ))
                    .map_err(draw_err)?;
            } else {
                chart
                    .draw_series(
                        sorted
                            .iter()
                            .map(|point| Circle::new((point.x, point.y), 4, color.filled())),
                    )
                    .map_err(draw_err)?;
            }
            // Text label offset: right+above by default. When the dot is in
            // the right 25% of the x range, flip the label left so it stays
            // inside the chart area. When near the bottom 15% of y, flip up
            // so the label isn't below the axis line.
            let x_flip_threshold = x_start + (x_end - x_start) * 0.75;
            let y_flip_threshold = measure_floor + (measure_ceiling - measure_floor) * 0.15;
            let label_style = ("sans-serif", 11).into_font().color(&BLACK);
            chart
                .draw_series(sorted.iter().map(|point| {
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
                    let x_off = if point.x >= x_flip_threshold {
                        -(char_px + 6)
                    } else {
                        6
                    };
                    let y_off = if point.y <= y_flip_threshold {
                        -20
                    } else {
                        -12
                    };
                    EmptyElement::at((point.x, point.y))
                        + Text::new(label, (x_off, y_off), label_style.clone())
                }))
                .map_err(draw_err)?;
        } else {
            // Default: dots/lines with legend entry.
            let annotation = if connect_points {
                chart
                    .draw_series(LineSeries::new(
                        sorted.iter().map(|point| (point.x, point.y)),
                        color.stroke_width(2),
                    ))
                    .map_err(draw_err)?
            } else {
                chart
                    .draw_series(
                        sorted
                            .iter()
                            .map(|point| Circle::new((point.x, point.y), 4, color.filled())),
                    )
                    .map_err(draw_err)?
            };
            if presentation.show_legend {
                if connect_points {
                    annotation.label(name).legend(move |(x, y)| {
                        PathElement::new(vec![(x, y), (x + 16, y)], color.stroke_width(2))
                    });
                } else {
                    annotation
                        .label(name)
                        .legend(move |(x, y)| Circle::new((x + 8, y), 4, color.filled()));
                }
            }
        }
        total_plotted += points.len();
    }

    // Only draw the legend box when label_points is off — with labels
    // on the dots, the legend is redundant and takes up chart space.
    if !opts.label_points && presentation.show_legend {
        draw_series_legend(&mut chart)?;
    }

    root.present().map_err(draw_err)?;
    Ok(total_plotted)
}

fn draw_line_or_scatter_log<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    groups: &ChartSeriesMap,
    opts: &ChartOptions,
    x_col: &str,
    y_col: &str,
    x_mode: XMode,
    title: &str,
    connect_points: bool,
    presentation: ChartPresentation,
) -> Result<usize, McpError>
where
    <DB as DrawingBackend>::ErrorType: 'static,
{
    let auto = bounds(groups);
    let x_pad = (auto.1 - auto.0).abs() * 0.05 + 1e-9;
    let (x_start, x_end) = match opts.x_range {
        Some([lo, hi]) => (lo, hi),
        None => (auto.0 - x_pad, auto.1 + x_pad),
    };
    let (x_start, x_end) =
        validate_effective_linear_range("effective x-axis range", (x_start, x_end))?;
    let values: Vec<f64> = groups
        .values()
        .flat_map(|points| points.iter().map(|point| point.y))
        .collect();
    let (measure_floor, measure_ceiling) = log_measure_range(&values, opts.y_range)?;

    let mut chart = ChartBuilder::on(root)
        .caption(title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(match x_mode {
            XMode::Categorical | XMode::Temporal(_) => 60,
            XMode::Numeric => 50,
        })
        .y_label_area_size(70)
        .build_cartesian_2d(
            x_start..x_end,
            (measure_floor..measure_ceiling)
                .log_scale()
                .with_key_points(bounded_log_key_points((measure_floor, measure_ceiling))),
        )
        .map_err(draw_err)?;

    match x_mode {
        XMode::Categorical => {
            let categories = collect_chart_categories(groups);
            let raw_labels: Vec<String> =
                categories.iter().map(|(_, label)| label.clone()).collect();
            let labels = strip_shared_tz_suffix(&raw_labels);
            let label_map = category_label_map(&categories, &labels);
            let tick_count = auto_tick_count(&labels, opts.width);
            chart
                .configure_mesh()
                .x_desc(x_col)
                .y_desc(y_col)
                .x_labels(tick_count)
                .x_label_formatter(&|value| category_label(*value, &label_map))
                .draw()
                .map_err(draw_err)?;
        }
        XMode::Temporal(kind) => {
            let sample = format_temporal_tick(x_start, kind);
            let sample_chars = sample.chars().count().max(10);
            let tick_count = tick_count_for_label_width(sample_chars, opts.width);
            chart
                .configure_mesh()
                .x_desc(x_col)
                .y_desc(y_col)
                .x_labels(tick_count)
                .x_label_formatter(&|value| format_temporal_tick(*value, kind))
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
    for (idx, (series_key, points)) in groups.iter().enumerate() {
        let color = series_color_for(series_key, idx, opts);
        let name = if series_key.is_empty() {
            y_col.to_string()
        } else {
            series_key.clone()
        };
        let mut sorted = points.clone();
        if connect_points {
            sorted.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        }

        if opts.label_points {
            if connect_points {
                chart
                    .draw_series(LineSeries::new(
                        sorted.iter().map(|point| (point.x, point.y)),
                        color.stroke_width(2),
                    ))
                    .map_err(draw_err)?;
            } else {
                chart
                    .draw_series(
                        sorted
                            .iter()
                            .map(|point| Circle::new((point.x, point.y), 4, color.filled())),
                    )
                    .map_err(draw_err)?;
            }
            let x_flip_threshold = x_start + (x_end - x_start) * 0.75;
            let y_flip_threshold =
                (measure_floor.ln() + (measure_ceiling.ln() - measure_floor.ln()) * 0.15).exp();
            let label_style = ("sans-serif", 11).into_font().color(&BLACK);
            chart
                .draw_series(sorted.iter().map(|point| {
                    let label = name.clone();
                    let char_px = i32::try_from(label.chars().count())
                        .unwrap_or(i32::MAX)
                        .saturating_mul(7);
                    let x_off = if point.x >= x_flip_threshold {
                        -(char_px + 6)
                    } else {
                        6
                    };
                    let y_off = if point.y <= y_flip_threshold {
                        -20
                    } else {
                        -12
                    };
                    EmptyElement::at((point.x, point.y))
                        + Text::new(label, (x_off, y_off), label_style.clone())
                }))
                .map_err(draw_err)?;
        } else {
            let annotation = if connect_points {
                chart
                    .draw_series(LineSeries::new(
                        sorted.iter().map(|point| (point.x, point.y)),
                        color.stroke_width(2),
                    ))
                    .map_err(draw_err)?
            } else {
                chart
                    .draw_series(
                        sorted
                            .iter()
                            .map(|point| Circle::new((point.x, point.y), 4, color.filled())),
                    )
                    .map_err(draw_err)?
            };
            if presentation.show_legend {
                if connect_points {
                    annotation.label(name).legend(move |(x, y)| {
                        PathElement::new(vec![(x, y), (x + 16, y)], color.stroke_width(2))
                    });
                } else {
                    annotation
                        .label(name)
                        .legend(move |(x, y)| Circle::new((x + 8, y), 4, color.filled()));
                }
            }
        }
        total_plotted += points.len();
    }

    if !opts.label_points && presentation.show_legend {
        draw_series_legend(&mut chart)?;
    }
    root.present().map_err(draw_err)?;
    Ok(total_plotted)
}

fn bounds(groups: &ChartSeriesMap) -> (f64, f64, f64, f64) {
    let (mut x_min, mut x_max) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut y_min, mut y_max) = (f64::INFINITY, f64::NEG_INFINITY);
    for pts in groups.values() {
        for point in pts {
            if point.x < x_min {
                x_min = point.x;
            }
            if point.x > x_max {
                x_max = point.x;
            }
            if point.y < y_min {
                y_min = point.y;
            }
            if point.y > y_max {
                y_max = point.y;
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
fn apply_ranges(
    auto: (f64, f64, f64, f64),
    opts: &ChartOptions,
) -> Result<(f64, f64, f64, f64), McpError> {
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
    let (final_x_min, final_x_max) =
        validate_effective_linear_range("effective x-axis range", (final_x_min, final_x_max))?;
    let (final_y_min, final_y_max) =
        validate_effective_linear_range("effective y-axis range", (final_y_min, final_y_max))?;
    Ok((final_x_min, final_x_max, final_y_min, final_y_max))
}

fn draw_histogram<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    rows: &[Value],
    opts: &ChartOptions,
    measures: Option<&[ChartMeasureValue]>,
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

    let mut values = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        match measures.and_then(|typed| typed.get(row_index)) {
            Some(ChartMeasureValue::Finite { coordinate, .. }) => values.push(*coordinate),
            Some(ChartMeasureValue::NonFinite) => {
                return Err(McpError::new(
                    ErrorCode::InvalidArgument,
                    format!("Column '{col}' contains a non-finite numeric value"),
                ));
            }
            Some(ChartMeasureValue::Null | ChartMeasureValue::NonNumeric) => {}
            None => {
                if let Some(value) = row
                    .as_object()
                    .and_then(|object| object.get(col))
                    .and_then(as_number)
                {
                    values.push(value);
                }
            }
        }
    }
    if values.is_empty() {
        return Err(McpError::new(
            ErrorCode::SchemaMismatch,
            format!("Column '{col}' has no numeric values to histogram"),
        ));
    }

    let bin_count = opts.bins.max(1) as usize;
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let raw_span = max - min;
    if !raw_span.is_finite() {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            "Histogram values do not form a finite representable span",
        ));
    }
    let span = if raw_span.abs() < 1e-12 {
        1.0
    } else {
        raw_span
    };
    let bin_width = span / bin_count as f64;
    if !bin_width.is_finite() || bin_width <= 0.0 {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            "Histogram bin width must be finite and strictly positive",
        ));
    }

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
    let histogram_end = max + bin_width * 0.01;
    let (histogram_start, histogram_end) =
        validate_effective_linear_range("effective histogram x-axis range", (min, histogram_end))?;
    let count_range = validate_effective_linear_range(
        "effective histogram count-axis range",
        (0.0, y_max * 1.1 + 1.0),
    )?;

    let mut chart = ChartBuilder::on(root)
        .caption(&title, ("sans-serif", 22))
        .margin(10)
        .x_label_area_size(50)
        .y_label_area_size(60)
        .build_cartesian_2d(histogram_start..histogram_end, count_range.0..count_range.1)
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

fn log_measure_range(values: &[f64], explicit: Option<[f64; 2]>) -> Result<(f64, f64), McpError> {
    if values.is_empty() {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            "A logarithmic measure scale requires at least one value",
        ));
    }
    if values
        .iter()
        .any(|value| !value.is_finite() || *value <= 0.0)
    {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            "A logarithmic measure scale requires every plotted value to be finite and strictly positive",
        ));
    }

    let data_min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let data_max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if let Some([lo, hi]) = explicit {
        let (lo, hi) = validate_log_coordinate_range("explicit logarithmic y_range", (lo, hi))?;
        if lo > data_min || hi < data_max {
            return Err(McpError::new(
                ErrorCode::InvalidArgument,
                "An explicit logarithmic y_range must contain every plotted value",
            ));
        }
        return Ok((lo, hi));
    }

    let positive_floor = f64::from_bits(1);
    let log_floor = positive_floor.ln();
    let log_ceiling = f64::MAX.ln();
    let log_min = data_min.ln();
    let log_max = data_max.ln();
    let pad = if log_max <= log_min {
        0.05 * std::f64::consts::LN_10
    } else {
        0.05 * (log_max - log_min)
    };
    let padded_log_lo = (log_min - pad).max(log_floor);
    let padded_log_hi = (log_max + pad).min(log_ceiling);

    let mut lo = if padded_log_lo <= log_floor {
        positive_floor
    } else {
        padded_log_lo.exp()
    };
    let mut hi = if padded_log_hi >= log_ceiling {
        f64::MAX
    } else {
        padded_log_hi.exp()
    };

    if lo > data_min || (lo.to_bits() == data_min.to_bits() && padded_log_lo < log_min) {
        lo = next_positive_down(data_min).unwrap_or(data_min);
    }
    if hi < data_max || (hi.to_bits() == data_max.to_bits() && padded_log_hi > log_max) {
        hi = next_positive_up(data_max).unwrap_or(data_max);
    }
    if lo >= hi {
        if let Some(expanded_lo) = next_positive_down(lo) {
            lo = expanded_lo;
        } else if let Some(expanded_hi) = next_positive_up(hi) {
            hi = expanded_hi;
        }
    }

    if lo > data_min || hi < data_max {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            "Could not construct a finite increasing logarithmic range that encloses every plotted value",
        ));
    }
    validate_log_coordinate_range("automatic logarithmic y range", (lo, hi))
}

fn validate_log_coordinate_range(name: &str, (lo, hi): (f64, f64)) -> Result<(f64, f64), McpError> {
    let log_lo = lo.ln();
    let log_hi = hi.ln();
    if !lo.is_finite()
        || !hi.is_finite()
        || lo <= 0.0
        || lo >= hi
        || !log_lo.is_finite()
        || !log_hi.is_finite()
        || log_lo >= log_hi
    {
        return Err(McpError::new(
            ErrorCode::InvalidArgument,
            format!(
                "The {name} must be finite, strictly positive, strictly increasing, and have a finite representable logarithmic span"
            ),
        ));
    }
    Ok((lo, hi))
}

fn next_positive_down(value: f64) -> Option<f64> {
    (value > f64::from_bits(1)).then(|| f64::from_bits(value.to_bits() - 1))
}

fn next_positive_up(value: f64) -> Option<f64> {
    (value < f64::MAX).then(|| f64::from_bits(value.to_bits() + 1))
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct SvgRect {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    }

    fn svg_i32_attr(line: &str, name: &str) -> Option<i32> {
        let marker = format!("{name}=\"");
        line.split_once(&marker)?.1.split_once('"')?.0.parse().ok()
    }

    fn svg_rects_with_fill(svg: &str, fill: &str) -> Vec<SvgRect> {
        svg.lines()
            .filter(|line| line.starts_with("<rect ") && line.contains(&format!("fill=\"{fill}\"")))
            .filter_map(|line| {
                Some(SvgRect {
                    x: svg_i32_attr(line, "x")?,
                    y: svg_i32_attr(line, "y")?,
                    width: svg_i32_attr(line, "width")?,
                    height: svg_i32_attr(line, "height")?,
                })
            })
            .collect()
    }

    fn svg_plot_y_bounds(svg: &str) -> Option<(i32, i32)> {
        svg.lines()
            .filter(|line| line.starts_with("<polyline ") && line.contains("stroke=\"#000000\""))
            .filter_map(|line| {
                let points = line.split_once("points=\"")?.1.split_once('"')?.0;
                let mut coords = points.split_whitespace().filter_map(|pair| {
                    let (x, y) = pair.split_once(',')?;
                    Some((x.parse::<i32>().ok()?, y.parse::<i32>().ok()?))
                });
                let first = coords.next()?;
                let second = coords.next()?;
                (first.0 == second.0).then_some((first.1.min(second.1), first.1.max(second.1)))
            })
            .max_by_key(|(top, bottom)| bottom - top)
    }

    fn bar_svg(
        rows: &[Value],
        x_as_category: Option<bool>,
        y_range: Option<[f64; 2]>,
    ) -> Result<String, McpError> {
        let opts = ChartOptions {
            chart_type: ChartType::Bar,
            x_column: Some("category".into()),
            y_column: Some("value".into()),
            format: ChartFormat::Svg,
            width: 400,
            height: 300,
            x_as_category,
            y_range,
            ..ChartOptions::default()
        };
        render_chart(rows, &opts).and_then(|result| {
            String::from_utf8(result.bytes).map_err(|error| {
                McpError::new(
                    ErrorCode::InternalError,
                    format!("renderer returned non-UTF-8 SVG: {error}"),
                )
            })
        })
    }

    const RANGE_VALIDATION_CHILD_ENV: &str = "HYPERDB_MCP_CHART_RANGE_VALIDATION_CHILD";

    fn assert_invalid_range_case(case: &str) {
        let ordinary_rows = vec![
            serde_json::json!({"category": 1.0, "value": 2.0}),
            serde_json::json!({"category": 2.0, "value": 3.0}),
        ];
        let (rows, chart_type, x_range, y_range) = match case {
            "reversed-x" => (ordinary_rows, ChartType::Bar, Some([2.0, 1.0]), None),
            "equal-y" => (ordinary_rows, ChartType::Bar, None, Some([2.0, 2.0])),
            "nan-x" => (ordinary_rows, ChartType::Line, Some([f64::NAN, 2.0]), None),
            "infinite-y" => (
                ordinary_rows,
                ChartType::Scatter,
                None,
                Some([0.0, f64::INFINITY]),
            ),
            "histogram-equal-x" => (ordinary_rows, ChartType::Histogram, Some([1.0, 1.0]), None),
            "finite-extreme-explicit-x" => (
                ordinary_rows,
                ChartType::Line,
                Some([f64::MIN, f64::MAX]),
                None,
            ),
            "finite-extreme-explicit-y" => (
                ordinary_rows,
                ChartType::Scatter,
                None,
                Some([f64::MIN, f64::MAX]),
            ),
            "line-auto-x-padding-overflow" => (
                vec![
                    serde_json::json!({"category": f64::MIN, "value": 2.0}),
                    serde_json::json!({"category": f64::MAX, "value": 3.0}),
                ],
                ChartType::Line,
                None,
                Some([1.0, 4.0]),
            ),
            "line-auto-y-padding-overflow" => (
                vec![
                    serde_json::json!({"category": 1.0, "value": f64::MIN}),
                    serde_json::json!({"category": 2.0, "value": f64::MAX}),
                ],
                ChartType::Line,
                Some([0.0, 3.0]),
                None,
            ),
            "bar-auto-y-padding-overflow" => (
                vec![
                    serde_json::json!({"category": "low", "value": f64::MIN}),
                    serde_json::json!({"category": "high", "value": f64::MAX}),
                ],
                ChartType::Bar,
                None,
                None,
            ),
            "histogram-auto-padding-overflow" => (
                vec![
                    serde_json::json!({"category": f64::MIN, "value": 1.0}),
                    serde_json::json!({"category": f64::MAX, "value": 2.0}),
                ],
                ChartType::Histogram,
                None,
                None,
            ),
            other => panic!("unknown chart range validation case {other}"),
        };
        let opts = ChartOptions {
            chart_type,
            x_column: Some("category".into()),
            y_column: Some("value".into()),
            format: ChartFormat::Svg,
            x_range,
            y_range,
            ..ChartOptions::default()
        };

        match render_chart(&rows, &opts) {
            Err(error) if error.code == ErrorCode::InvalidArgument => {}
            Err(error) => panic!(
                "{case}: expected InvalidArgument, got {:?}: {}",
                error.code, error.message
            ),
            Ok(_) => panic!("{case}: unsafe effective range was accepted"),
        }
    }

    fn record_bounded_range_case(failures: &mut Vec<String>, case: &str) {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let mut child = Command::new(std::env::current_exe().expect("unit test executable path"))
            .args([
                "--exact",
                "chart::tests::bar_ranges_and_categories_are_validated",
                "--nocapture",
            ])
            .env(RANGE_VALIDATION_CHILD_ENV, case)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("range validation parent must spawn its exact helper child");

        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let output = child
                        .wait_with_output()
                        .expect("range validation parent must collect child output");
                    if !status.success() {
                        failures.push(format!(
                            "{case}: validation child failed with {status}\nstdout:\n{}\nstderr:\n{}",
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        ));
                    }
                    return;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let kill_error = child.kill().err();
                    let output = child
                        .wait_with_output()
                        .expect("range validation parent must wait for timed-out child");
                    failures.push(format!(
                        "{case}: renderer exceeded the 4s pre-validation bound and was killed ({kill_error:?})\nstdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                    return;
                }
                Err(error) => {
                    let _ = child.kill();
                    let output = child
                        .wait_with_output()
                        .expect("range validation parent must wait after status error");
                    failures.push(format!(
                        "{case}: validation child status failed: {error}\nstdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                    return;
                }
            }
        }
    }

    /// Mutations caught: honoring `x_as_category:false` for bars (which pushes
    /// numeric categories off-canvas), ignoring the fixed measure range, using
    /// zero outside a positive-/negative-only range, accepting malformed or
    /// arithmetic-unsafe explicit ranges, and allowing derived linear padding
    /// or histogram spans to overflow before Plotters sees them.
    #[test]
    fn bar_ranges_and_categories_are_validated() {
        if let Ok(case) = std::env::var(RANGE_VALIDATION_CHILD_ENV) {
            assert_invalid_range_case(&case);
            return;
        }

        let mut failures = Vec::new();

        let numeric_rows = vec![
            serde_json::json!({"category": 1000, "value": 12}),
            serde_json::json!({"category": 2000, "value": 18}),
        ];
        match bar_svg(&numeric_rows, Some(false), None) {
            Ok(svg) => {
                let bars: Vec<_> = svg_rects_with_fill(&svg, "#1F77B4")
                    .into_iter()
                    .filter(|rect| rect.width > 20 && rect.height > 0)
                    .collect();
                if bars.len() != 2 {
                    failures.push(format!(
                        "numeric bar x values must remain categorical even with x_as_category:false; expected two visible bars, got {bars:?}"
                    ));
                }
            }
            Err(error) => failures.push(format!(
                "numeric categorical bar render unexpectedly failed: {error}"
            )),
        }

        for (case, value, range, baseline_at_bottom) in [
            ("positive-only", 15.0, [10.0, 20.0], true),
            ("negative-only", -15.0, [-20.0, -10.0], false),
        ] {
            let rows = vec![serde_json::json!({"category": "A", "value": value})];
            match bar_svg(&rows, None, Some(range)) {
                Ok(svg) => {
                    let Some((plot_top, plot_bottom)) = svg_plot_y_bounds(&svg) else {
                        failures.push(format!("{case}: could not locate SVG plot bounds"));
                        continue;
                    };
                    let Some(bar) = svg_rects_with_fill(&svg, "#1F77B4")
                        .into_iter()
                        .max_by_key(|rect| rect.width.saturating_mul(rect.height))
                    else {
                        failures.push(format!("{case}: no bar rectangle was rendered"));
                        continue;
                    };
                    let baseline = if baseline_at_bottom {
                        bar.y + bar.height
                    } else {
                        bar.y
                    };
                    let expected = if baseline_at_bottom {
                        plot_bottom
                    } else {
                        plot_top
                    };
                    if (baseline - expected).abs() > 2 {
                        failures.push(format!(
                            "{case}: explicit y_range {range:?} must anchor the bar at its nearer boundary {expected}, got rectangle {bar:?} within plot {plot_top}..{plot_bottom}"
                        ));
                    }
                }
                Err(error) => failures.push(format!("{case}: render unexpectedly failed: {error}")),
            }
        }

        for case in [
            "reversed-x",
            "equal-y",
            "nan-x",
            "infinite-y",
            "histogram-equal-x",
            "finite-extreme-explicit-x",
            "finite-extreme-explicit-y",
            "line-auto-x-padding-overflow",
            "line-auto-y-padding-overflow",
            "bar-auto-y-padding-overflow",
            "histogram-auto-padding-overflow",
        ] {
            record_bounded_range_case(&mut failures, case);
        }

        assert!(
            failures.is_empty(),
            "bar range/category contract failures:\n{}",
            failures.join("\n")
        );
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SvgText {
        x: i32,
        y: i32,
        text: String,
        opening_tag: String,
    }

    fn svg_text_elements(svg: &str) -> Vec<SvgText> {
        let lines: Vec<_> = svg.lines().collect();
        let mut elements = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            let opening = lines[index];
            if !opening.starts_with("<text ") {
                index += 1;
                continue;
            }
            let Some(x) = svg_i32_attr(opening, "x") else {
                index += 1;
                continue;
            };
            let Some(y) = svg_i32_attr(opening, "y") else {
                index += 1;
                continue;
            };
            let mut content = Vec::new();
            index += 1;
            while index < lines.len() && !lines[index].contains("</text>") {
                content.push(lines[index].trim());
                index += 1;
            }
            elements.push(SvgText {
                x,
                y,
                text: content.join("\n"),
                opening_tag: opening.to_string(),
            });
            index += 1;
        }
        elements
    }

    fn primary_bar_rects(svg: &str) -> Vec<(&str, SvgRect)> {
        const FILLS: [&str; 8] = [
            "#1F77B4", "#FF7F0E", "#2CA02C", "#D62728", "#9467BD", "#8C564B", "#E377C2", "#7F7F7F",
        ];
        let mut rectangles = Vec::new();
        for fill in FILLS {
            rectangles.extend(
                svg_rects_with_fill(svg, fill)
                    .into_iter()
                    .filter(|rect| rect.width > 0 && rect.height > 10)
                    .map(|rect| (fill, rect)),
            );
        }
        rectangles
    }

    fn chart_svg_with_presentation(
        rows: &[Value],
        chart_type: ChartType,
        series_column: Option<&str>,
        label_points: bool,
        y_range: Option<[f64; 2]>,
        presentation: ChartPresentation,
    ) -> Result<String, McpError> {
        let opts = ChartOptions {
            chart_type,
            x_column: Some("category".into()),
            y_column: Some("value".into()),
            series_column: series_column.map(str::to_string),
            format: ChartFormat::Svg,
            width: 520,
            height: 360,
            label_points,
            y_range,
            ..ChartOptions::default()
        };
        render_chart_with_presentation(rows, &opts, presentation).and_then(|result| {
            String::from_utf8(result.bytes).map_err(|error| {
                McpError::new(
                    ErrorCode::InternalError,
                    format!("renderer returned non-UTF-8 SVG: {error}"),
                )
            })
        })
    }

    /// Mutations caught: reordering categories or series, deduplicating rows,
    /// filling absent category/series cells, clipping label text in the data
    /// model, drawing horizontal bars on the old axes, or placing the first SQL
    /// category at the bottom.
    #[test]
    fn horizontal_bar_layout_contract() {
        let long_first = format!(
            "First SQL category — 東京 — {}",
            "wide ".repeat(14).trim_end()
        );
        let rows = vec![
            serde_json::json!({"category": long_first, "value": 10, "series": "B"}),
            serde_json::json!({"category": long_first, "value": 20, "series": "A"}),
            serde_json::json!({"category": long_first, "value": 25, "series": "A"}),
            serde_json::json!({"category": "Second category", "value": 30, "series": "A"}),
        ];

        // Characterize the existing grouping contract before asking the
        // horizontal renderer to consume it.
        let groups = group_series(
            &rows,
            "category",
            "value",
            Some("series"),
            XMode::Categorical,
        )
        .expect("legacy category grouping must succeed");
        assert_eq!(
            groups.keys().map(String::as_str).collect::<Vec<_>>(),
            ["A", "B"]
        );
        let a_points = &groups["A"];
        assert_eq!(
            a_points
                .iter()
                .map(|(x, y, label)| (*x, *y, label.as_str()))
                .collect::<Vec<_>>(),
            [
                (0.0, 20.0, long_first.as_str()),
                (0.0, 25.0, long_first.as_str()),
                (1.0, 30.0, "Second category"),
            ],
            "duplicate category+series rows must remain in input order"
        );
        assert_eq!(
            groups["B"].len(),
            1,
            "missing B/second cell must remain a gap"
        );
        assert_eq!(
            collect_categories(&groups)
                .into_iter()
                .map(|(_, label)| label)
                .collect::<Vec<_>>(),
            [long_first.clone(), "Second category".to_string()],
            "categories must retain first-seen SQL order"
        );
        assert_eq!(
            series_color(0),
            series_color(8),
            "the eight-color palette must cycle"
        );
        assert_ne!(series_color(0), series_color(1));

        let mut failures = Vec::new();
        let horizontal = ChartPresentation {
            bar_orientation: BarOrientation::Horizontal,
            show_legend: false,
            ..ChartPresentation::default()
        };
        match chart_svg_with_presentation(
            &rows,
            ChartType::Bar,
            Some("series"),
            false,
            None,
            horizontal,
        ) {
            Ok(svg) => {
                let texts = svg_text_elements(&svg);
                let first = texts.iter().find(|element| element.text == long_first);
                let second = texts
                    .iter()
                    .find(|element| element.text == "Second category");
                match (first, second) {
                    (Some(first), Some(second)) if first.y < second.y => {}
                    (Some(first), Some(second)) => failures.push(format!(
                        "first SQL category must be above the second: first={first:?}, second={second:?}"
                    )),
                    _ => failures.push(format!(
                        "horizontal SVG must retain full long/Unicode labels; texts={texts:?}"
                    )),
                }
                if !texts.iter().any(|element| {
                    element.text == "category" && element.opening_tag.contains("rotate(270")
                }) || !texts.iter().any(|element| {
                    element.text == "value" && !element.opening_tag.contains("rotate(270")
                }) {
                    failures.push(
                        "horizontal axes must describe category vertically and value horizontally"
                            .into(),
                    );
                }
                if !svg.starts_with("<svg width=\"520\" height=\"360\"") {
                    failures.push("long labels must not trigger implicit chart auto-sizing".into());
                }

                let mut bars = primary_bar_rects(&svg);
                bars.sort_by_key(|(_, rect)| (rect.y, rect.x, rect.width));
                if bars.len() != 4 {
                    failures.push(format!(
                        "expected four marks with the missing B/second cell left empty, got {bars:?}"
                    ));
                }
                let duplicate_pair = bars.iter().enumerate().any(|(left_index, (_, left))| {
                    bars.iter().skip(left_index + 1).any(|(_, right)| {
                        left.y == right.y
                            && left.height == right.height
                            && left.x == right.x
                            && left.width != right.width
                    })
                });
                if !duplicate_pair {
                    failures.push(format!(
                        "duplicate category+series rows must remain distinct overlapping marks in input order: {bars:?}"
                    ));
                }
            }
            Err(error) => failures.push(format!("horizontal SVG render failed: {error}")),
        }

        let one_row = vec![serde_json::json!({"category": "Only", "value": 7})];
        match chart_svg_with_presentation(&one_row, ChartType::Bar, None, false, None, horizontal) {
            Ok(svg) if primary_bar_rects(&svg).len() == 1 => {}
            Ok(svg) => failures.push(format!(
                "one-category horizontal bar must render exactly one mark: {:?}",
                primary_bar_rects(&svg)
            )),
            Err(error) => failures.push(format!(
                "one-category horizontal bar unexpectedly failed: {error}"
            )),
        }

        assert!(
            failures.is_empty(),
            "horizontal bar layout failures:\n{}",
            failures.join("\n")
        );
    }

    /// A fixed-height horizontal chart must not emit one full SVG text/tick
    /// node per SQL category. Twelve pixels is a conservative minimum pitch
    /// for the renderer's roughly ten-pixel label font; using the full image
    /// height (rather than only the smaller plot area) keeps this upper bound
    /// intentionally generous.
    #[test]
    fn horizontal_bar_category_labels_are_pixel_bounded() {
        const CATEGORY_COUNT: usize = 256;
        const SVG_HEIGHT_PX: usize = 360;
        const MIN_LABEL_PITCH_PX: usize = 12;
        const LABEL_LIMIT: usize = SVG_HEIGHT_PX / MIN_LABEL_PITCH_PX + 2;

        let rows: Vec<_> = (0..CATEGORY_COUNT)
            .map(|index| {
                serde_json::json!({
                    "category": format!("category-{index:03} — 東京 — {}", "wide".repeat(8)),
                    "value": index + 1,
                })
            })
            .collect();
        let horizontal = ChartPresentation {
            bar_orientation: BarOrientation::Horizontal,
            show_legend: false,
            ..ChartPresentation::default()
        };
        let svg = chart_svg_with_presentation(&rows, ChartType::Bar, None, false, None, horizontal)
            .expect("many-category horizontal SVG must render");
        let category_label_count = svg_text_elements(&svg)
            .iter()
            .filter(|element| element.text.starts_with("category-"))
            .count();
        assert!(
            category_label_count <= LABEL_LIMIT,
            "fixed {SVG_HEIGHT_PX}px horizontal SVG emitted {category_label_count} full category labels for {CATEGORY_COUNT} rows; pixel-derived upper bound is {LABEL_LIMIT}"
        );
    }

    /// Mutations caught: changing the legacy legend default, drawing a legend
    /// when suppressed (including `label_points`), formatting values from the
    /// converted f64 instead of their original scalar, or implementing SVG but
    /// omitting the horizontal PNG backend.
    #[test]
    fn legend_and_value_label_contract() {
        let rows = vec![
            serde_json::json!({"category": "北", "value": 12345, "series": "Series α"}),
            serde_json::json!({"category": "南", "value": -678, "series": "Series β"}),
        ];
        let mut failures = Vec::new();

        match chart_svg_with_presentation(
            &rows,
            ChartType::Bar,
            Some("series"),
            false,
            Some([-1000.0, 20_000.0]),
            ChartPresentation::default(),
        ) {
            Ok(svg)
                if svg.contains("Series α")
                    && svg.contains("Series β")
                    && svg.contains("opacity=\"0.9\" fill=\"#FFFFFF\"") => {}
            Ok(_) => failures.push("legacy/default bar presentation must keep its legend".into()),
            Err(error) => failures.push(format!("legacy/default SVG render failed: {error}")),
        }

        let labels_without_legend = ChartPresentation {
            label_values: true,
            show_legend: false,
            ..ChartPresentation::default()
        };
        match chart_svg_with_presentation(
            &rows,
            ChartType::Bar,
            Some("series"),
            false,
            Some([-1000.0, 20_000.0]),
            labels_without_legend,
        ) {
            Ok(svg) => {
                let texts = svg_text_elements(&svg);
                for exact in ["12345", "-678", "北", "南"] {
                    if !texts.iter().any(|element| element.text == exact) {
                        failures.push(format!(
                            "value/category scalar {exact:?} must survive unchanged in SVG text"
                        ));
                    }
                }
                if svg.contains("Series α")
                    || svg.contains("Series β")
                    || svg.contains("opacity=\"0.9\" fill=\"#FFFFFF\"")
                {
                    failures.push("show_legend:false must suppress the complete bar legend".into());
                }
            }
            Err(error) => failures.push(format!(
                "bar value-label/legend-suppression render failed: {error}"
            )),
        }

        for chart_type in [ChartType::Line, ChartType::Scatter] {
            let numeric_rows = vec![
                serde_json::json!({"category": 1, "value": 2, "series": "Hidden α"}),
                serde_json::json!({"category": 2, "value": 3, "series": "Hidden β"}),
            ];
            let presentation = ChartPresentation {
                show_legend: false,
                ..ChartPresentation::default()
            };
            match chart_svg_with_presentation(
                &numeric_rows,
                chart_type,
                Some("series"),
                false,
                None,
                presentation,
            ) {
                Ok(svg)
                    if !svg.contains("Hidden α")
                        && !svg.contains("Hidden β")
                        && !svg.contains("opacity=\"0.9\" fill=\"#FFFFFF\"") => {}
                Ok(_) => failures.push(format!(
                    "show_legend:false must suppress the {chart_type:?} legend"
                )),
                Err(error) => failures.push(format!(
                    "show_legend:false {chart_type:?} render failed: {error}"
                )),
            }
        }

        let point_label_rows = vec![
            serde_json::json!({"category": 1, "value": 2, "series": "Point α"}),
            serde_json::json!({"category": 2, "value": 3, "series": "Point β"}),
        ];
        match chart_svg_with_presentation(
            &point_label_rows,
            ChartType::Line,
            Some("series"),
            true,
            None,
            ChartPresentation::default(),
        ) {
            Ok(svg) if !svg.contains("opacity=\"0.9\" fill=\"#FFFFFF\"") => {}
            Ok(_) => failures.push("label_points:true must suppress the line legend".into()),
            Err(error) => failures.push(format!("point-label SVG render failed: {error}")),
        }

        let png_opts = ChartOptions {
            chart_type: ChartType::Bar,
            x_column: Some("category".into()),
            y_column: Some("value".into()),
            series_column: Some("series".into()),
            format: ChartFormat::Png,
            width: 360,
            height: 260,
            y_range: Some([-1000.0, 20_000.0]),
            ..ChartOptions::default()
        };
        let horizontal_png = ChartPresentation {
            bar_orientation: BarOrientation::Horizontal,
            label_values: true,
            show_legend: false,
            y_scale: MeasureScale::Linear,
        };
        match render_chart_with_presentation(&rows, &png_opts, horizontal_png) {
            Ok(result)
                if result.mime_type == "image/png"
                    && result
                        .bytes
                        .starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) => {}
            Ok(result) => failures.push(format!(
                "horizontal PNG must preserve PNG MIME/magic, got {} and {:?}",
                result.mime_type,
                result.bytes.get(..8)
            )),
            Err(error) => failures.push(format!("horizontal PNG render failed: {error}")),
        }

        assert!(
            failures.is_empty(),
            "legend/value-label failures:\n{}",
            failures.join("\n")
        );
    }

    fn close_enough(actual: f64, expected: f64) -> bool {
        let scale = expected.abs().max(1.0);
        (actual - expected).abs() <= scale * 1e-12
    }

    /// Mutations caught: computing padding in linear space, collapsing an
    /// equal-value domain, flushing the minimum subnormal to zero, overflowing
    /// the maximum finite bound, and accepting a non-positive/non-containing
    /// explicit logarithmic range.
    #[test]
    fn log_range_handles_finite_extremes() {
        let mut failures = Vec::new();

        match log_measure_range(&[1.0, 100.0], None) {
            Ok((lo, hi))
                if close_enough(lo, 0.794_328_234_724_281_5)
                    && close_enough(hi, 125.892_541_179_416_75) => {}
            Ok(actual) => failures.push(format!(
                "1..100 auto log range must apply five-percent ln-span padding, got {actual:?}"
            )),
            Err(error) => failures.push(format!("ordinary auto log range failed: {error}")),
        }

        match log_measure_range(&[10.0, 10.0], None) {
            Ok((lo, hi))
                if close_enough(lo, 8.912_509_381_337_454)
                    && close_enough(hi, 11.220_184_543_019_634) => {}
            Ok(actual) => failures.push(format!(
                "repeated value must use a fixed five-percent decade pad, got {actual:?}"
            )),
            Err(error) => failures.push(format!("repeated-value log range failed: {error}")),
        }

        let smallest = f64::from_bits(1);
        match log_measure_range(&[smallest], None) {
            Ok((lo, hi))
                if lo.to_bits() == smallest.to_bits()
                    && hi.is_finite()
                    && hi.is_sign_positive()
                    && hi.to_bits() > smallest.to_bits() => {}
            Ok(actual) => failures.push(format!(
                "minimum-subnormal range must retain the value and expand on the available side, got {actual:?}"
            )),
            Err(error) => failures.push(format!("minimum-subnormal log range failed: {error}")),
        }

        match log_measure_range(&[f64::MAX], None) {
            Ok((lo, hi))
                if hi.to_bits() == f64::MAX.to_bits()
                    && lo.is_finite()
                    && lo > 0.0
                    && lo < hi => {}
            Ok(actual) => failures.push(format!(
                "maximum-finite range must retain the value and expand on the available side, got {actual:?}"
            )),
            Err(error) => failures.push(format!("maximum-finite log range failed: {error}")),
        }

        match log_measure_range(&[smallest, f64::MAX], None) {
            Ok((lo, hi))
                if lo.to_bits() == smallest.to_bits()
                    && hi.to_bits() == f64::MAX.to_bits()
                    && lo < hi => {}
            Ok(actual) => failures.push(format!(
                "full finite-positive domain must clamp without excluding either extreme, got {actual:?}"
            )),
            Err(error) => failures.push(format!("full-domain log range failed: {error}")),
        }

        match log_measure_range(&[10.0, 100.0], Some([1.0, 1000.0])) {
            Ok((lo, hi))
                if lo.to_bits() == 1.0_f64.to_bits() && hi.to_bits() == 1000.0_f64.to_bits() => {}
            Ok(actual) => failures.push(format!(
                "valid explicit log range must be preserved exactly, got {actual:?}"
            )),
            Err(error) => failures.push(format!("valid explicit log range failed: {error}")),
        }

        for (case, values, explicit) in [
            ("zero", vec![0.0], None),
            ("negative", vec![-1.0], None),
            ("mixed sign", vec![-1.0, 1.0], None),
            ("NaN", vec![f64::NAN], None),
            ("infinity", vec![f64::INFINITY], None),
            ("reversed explicit", vec![10.0], Some([100.0, 1.0])),
            ("equal explicit", vec![10.0], Some([10.0, 10.0])),
            ("zero explicit", vec![10.0], Some([0.0, 100.0])),
            (
                "non-finite explicit",
                vec![10.0],
                Some([1.0, f64::INFINITY]),
            ),
            (
                "explicit excludes low value",
                vec![10.0, 100.0],
                Some([20.0, 200.0]),
            ),
            (
                "explicit excludes high value",
                vec![10.0, 100.0],
                Some([1.0, 50.0]),
            ),
        ] {
            match log_measure_range(&values, explicit) {
                Err(error) if error.code == ErrorCode::InvalidArgument => {}
                Err(error) => failures.push(format!(
                    "{case}: expected InvalidArgument, got {:?}: {}",
                    error.code, error.message
                )),
                Ok(range) => failures.push(format!(
                    "{case}: invalid log domain was accepted as {range:?}"
                )),
            }
        }

        assert!(
            failures.is_empty(),
            "log range failures:\n{}",
            failures.join("\n")
        );
    }

    fn svg_plot_x_bounds(svg: &str) -> Option<(i32, i32)> {
        svg.lines()
            .filter(|line| line.starts_with("<polyline ") && line.contains("stroke=\"#000000\""))
            .filter_map(|line| {
                let points = line.split_once("points=\"")?.1.split_once('"')?.0;
                let mut coords = points.split_whitespace().filter_map(|pair| {
                    let (x, y) = pair.split_once(',')?;
                    Some((x.parse::<i32>().ok()?, y.parse::<i32>().ok()?))
                });
                let first = coords.next()?;
                let second = coords.next()?;
                (first.1 == second.1).then_some((first.0.min(second.0), first.0.max(second.0)))
            })
            .max_by_key(|(left, right)| right - left)
    }

    fn log_presentation(bar_orientation: BarOrientation) -> ChartPresentation {
        ChartPresentation {
            bar_orientation,
            y_scale: MeasureScale::Log,
            ..ChartPresentation::default()
        }
    }

    const LOG_RENDERING_CHILD_ENV: &str = "HYPERDB_MCP_LOG_RENDERING_CHILD";
    const LOG_RENDERING_AGGREGATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(28);

    fn assert_full_domain_log_semantics() {
        let mut failures = Vec::new();
        let rows = vec![
            serde_json::json!({"category": "smallest positive", "value": f64::from_bits(1)}),
            serde_json::json!({"category": "one", "value": 1.0}),
            serde_json::json!({"category": "largest finite", "value": f64::MAX}),
        ];
        match chart_svg_with_presentation(
            &rows,
            ChartType::Bar,
            None,
            false,
            None,
            log_presentation(BarOrientation::Vertical),
        ) {
            Ok(svg) => {
                let texts = svg_text_elements(&svg);
                for expected in ["smallest positive", "one", "largest finite"] {
                    if !texts.iter().any(|element| element.text == expected) {
                        failures.push(format!(
                            "full-domain log chart must retain category label {expected:?}"
                        ));
                    }
                }

                let category_desc = texts.iter().find(|element| {
                    element.text == "category" && !element.opening_tag.contains("rotate(270")
                });
                let value_desc = texts.iter().find(|element| {
                    element.text == "value" && element.opening_tag.contains("rotate(270")
                });
                if category_desc.is_none() || value_desc.is_none() {
                    failures.push(format!(
                        "full-domain vertical log chart must render category-x/value-y axis descriptions, got category={category_desc:?}, value={value_desc:?}"
                    ));
                }

                let measure_ticks: Vec<_> = texts
                    .iter()
                    .filter_map(|element| element.text.parse::<f64>().ok())
                    .filter(|value| value.is_finite() && *value > 0.0)
                    .collect();
                if measure_ticks.len() < 2 {
                    failures.push(format!(
                        "full-domain log chart must retain normal positive measure-scale tick labels, got {measure_ticks:?}"
                    ));
                }

                let mut heights: Vec<_> = primary_bar_rects(&svg)
                    .into_iter()
                    .map(|(_, rect)| rect.height)
                    .collect();
                heights.sort_unstable();
                match heights.as_slice() {
                    [middle, maximum] if *middle > 0 => {
                        let ratio = f64::from(*maximum) / f64::from(*middle);
                        if !(1.7..=2.3).contains(&ratio) {
                            failures.push(format!(
                                "full-domain log geometry must place 1.0 near the logarithmic midpoint between the minimum subnormal and f64::MAX; got heights {heights:?}, ratio={ratio}"
                            ));
                        }
                    }
                    _ => failures.push(format!(
                        "full-domain log chart must contain visible midpoint and maximum bars, got heights {heights:?}"
                    )),
                }
            }
            Err(error) => failures.push(format!("full-domain semantic log SVG failed: {error}")),
        }

        assert!(
            failures.is_empty(),
            "full-domain log semantic failures:\n{}",
            failures.join("\n")
        );
    }

    fn assert_explicit_adjacent_high_log_rejected() {
        let lower = f64::from_bits(f64::MAX.to_bits() - 1);
        let rows = vec![
            serde_json::json!({"category": "lower", "value": lower}),
            serde_json::json!({"category": "maximum", "value": f64::MAX}),
        ];
        match chart_svg_with_presentation(
            &rows,
            ChartType::Bar,
            None,
            false,
            Some([lower, f64::MAX]),
            log_presentation(BarOrientation::Vertical),
        ) {
            Err(error) if error.code == ErrorCode::InvalidArgument => {}
            Err(error) => panic!(
                "adjacent high-end explicit log range must return InvalidArgument, got {:?}: {}",
                error.code, error.message
            ),
            Ok(_) => panic!(
                "adjacent high-end explicit log endpoints with equal logarithms were accepted"
            ),
        }
    }

    fn assert_auto_adjacent_high_log_expands() {
        let lower = f64::from_bits(f64::MAX.to_bits() - 1);
        match log_measure_range(&[lower, f64::MAX], None) {
            Ok((lo, hi))
                if lo <= lower
                    && hi >= f64::MAX
                    && lo.ln().is_finite()
                    && hi.ln().is_finite()
                    && lo.ln() < hi.ln() => {}
            Ok(range) => panic!(
                "automatic adjacent high-end values must expand to distinct finite logarithmic endpoints without exclusion, got {range:?} with logs {:?}",
                (range.0.ln(), range.1.ln())
            ),
            Err(error) => panic!(
                "automatic adjacent high-end values must follow repeated-value expansion policy, got {:?}: {}",
                error.code, error.message
            ),
        }

        let rows = vec![
            serde_json::json!({"category": "lower", "value": lower}),
            serde_json::json!({"category": "maximum", "value": f64::MAX}),
        ];
        match chart_svg_with_presentation(
            &rows,
            ChartType::Bar,
            None,
            false,
            None,
            log_presentation(BarOrientation::Vertical),
        ) {
            Ok(svg) if svg.starts_with("<svg") => {}
            Ok(_) => panic!("automatic adjacent high-end log render returned malformed SVG"),
            Err(error) => panic!("automatic adjacent high-end log render failed: {error}"),
        }
    }

    fn assert_finite_extremes_line_renders() {
        let rows = vec![
            serde_json::json!({"category": 1, "value": f64::from_bits(1)}),
            serde_json::json!({"category": 2, "value": f64::MAX}),
        ];
        match chart_svg_with_presentation(
            &rows,
            ChartType::Line,
            None,
            false,
            None,
            log_presentation(BarOrientation::Vertical),
        ) {
            Ok(svg) if svg.starts_with("<svg") => {}
            Ok(_) => panic!("finite extremes: renderer returned malformed SVG"),
            Err(error) => panic!("finite extremes: log SVG failed: {error}"),
        }
    }

    fn run_log_rendering_child_case(case: &str) {
        match case {
            "finite-extremes-line" => assert_finite_extremes_line_renders(),
            "full-domain-semantics" => assert_full_domain_log_semantics(),
            "explicit-adjacent-high" => assert_explicit_adjacent_high_log_rejected(),
            "auto-adjacent-high" => assert_auto_adjacent_high_log_expands(),
            other => panic!("unknown bounded log-rendering case {other}"),
        }
    }

    fn record_bounded_log_rendering_case(
        failures: &mut Vec<String>,
        case: &str,
        aggregate_deadline: std::time::Instant,
    ) {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        if Instant::now() >= aggregate_deadline {
            failures.push(format!(
                "{case}: shared {}s log-rendering deadline elapsed before child launch",
                LOG_RENDERING_AGGREGATE_TIMEOUT.as_secs()
            ));
            return;
        }

        let mut child = Command::new(std::env::current_exe().expect("unit test executable path"))
            .args([
                "--exact",
                "chart::tests::log_rendering_contract",
                "--nocapture",
            ])
            .env(LOG_RENDERING_CHILD_ENV, case)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("log rendering parent must spawn its exact helper child");

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let output = child
                        .wait_with_output()
                        .expect("log rendering parent must collect child output");
                    if !status.success() {
                        failures.push(format!(
                            "{case}: child failed with {status}\nstdout:\n{}\nstderr:\n{}",
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        ));
                    }
                    return;
                }
                Ok(None) if Instant::now() < aggregate_deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let kill_error = child.kill().err();
                    let output = child
                        .wait_with_output()
                        .expect("log rendering parent must wait for timed-out child");
                    failures.push(format!(
                        "{case}: renderer exceeded the shared {}s log-rendering deadline and was killed ({kill_error:?})\nstdout:\n{}\nstderr:\n{}",
                        LOG_RENDERING_AGGREGATE_TIMEOUT.as_secs(),
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                    return;
                }
                Err(error) => {
                    let _ = child.kill();
                    let output = child
                        .wait_with_output()
                        .expect("log rendering parent must wait after status error");
                    failures.push(format!(
                        "{case}: child status failed: {error}\nstdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                    return;
                }
            }
        }
    }

    /// Mutations caught: applying log to the physical rather than data-role y
    /// axis, starting log bars at numeric zero, inverted rectangles, omitting a
    /// backend/orientation branch, silently filtering invalid values, and
    /// permitting log histograms or ranges that exclude plotted data.
    #[test]
    fn log_rendering_contract() {
        if let Ok(case) = std::env::var(LOG_RENDERING_CHILD_ENV) {
            run_log_rendering_child_case(&case);
            return;
        }

        let mut failures = Vec::new();
        let bar_rows = vec![
            serde_json::json!({"category": "First", "value": 10.0}),
            serde_json::json!({"category": "Second", "value": 100.0}),
        ];

        match chart_svg_with_presentation(
            &bar_rows,
            ChartType::Bar,
            None,
            false,
            Some([1.0, 1000.0]),
            log_presentation(BarOrientation::Vertical),
        ) {
            Ok(svg) => {
                if let Some((_, plot_bottom)) = svg_plot_y_bounds(&svg) {
                    let bars = primary_bar_rects(&svg);
                    if bars.len() != 2
                        || bars.iter().any(|(_, rect)| {
                            rect.width <= 0
                                || rect.height <= 0
                                || (rect.y + rect.height - plot_bottom).abs() > 2
                        })
                    {
                        failures.push(format!(
                            "vertical log bars must be non-inverted and start at the positive lower bound {plot_bottom}: {bars:?}"
                        ));
                    }
                } else {
                    failures.push("vertical log bar SVG is missing plot bounds".into());
                }
            }
            Err(error) => failures.push(format!("vertical log bar SVG failed: {error}")),
        }

        match chart_svg_with_presentation(
            &bar_rows,
            ChartType::Bar,
            None,
            false,
            Some([1.0, 1000.0]),
            log_presentation(BarOrientation::Horizontal),
        ) {
            Ok(svg) => {
                if let Some((plot_left, _)) = svg_plot_x_bounds(&svg) {
                    let bars = primary_bar_rects(&svg);
                    if bars.len() != 2
                        || bars.iter().any(|(_, rect)| {
                            rect.width <= 0 || rect.height <= 0 || (rect.x - plot_left).abs() > 2
                        })
                    {
                        failures.push(format!(
                            "horizontal log bars must map data-role y to physical x and start at positive lower bound {plot_left}: {bars:?}"
                        ));
                    }
                } else {
                    failures.push("horizontal log bar SVG is missing plot bounds".into());
                }
            }
            Err(error) => failures.push(format!("horizontal log bar SVG failed: {error}")),
        }

        for (case, chart_type, rows) in [
            (
                "positive line",
                ChartType::Line,
                vec![
                    serde_json::json!({"category": 1, "value": 1.0}),
                    serde_json::json!({"category": 2, "value": 10.0}),
                    serde_json::json!({"category": 3, "value": 100.0}),
                ],
            ),
            (
                "positive scatter",
                ChartType::Scatter,
                vec![
                    serde_json::json!({"category": 1, "value": 1.0}),
                    serde_json::json!({"category": 2, "value": 10.0}),
                ],
            ),
            (
                "repeated value",
                ChartType::Line,
                vec![
                    serde_json::json!({"category": 1, "value": 10.0}),
                    serde_json::json!({"category": 2, "value": 10.0}),
                ],
            ),
        ] {
            match chart_svg_with_presentation(
                &rows,
                chart_type,
                None,
                false,
                None,
                log_presentation(BarOrientation::Vertical),
            ) {
                Ok(svg) if svg.starts_with("<svg") => {}
                Ok(_) => failures.push(format!("{case}: renderer returned malformed SVG")),
                Err(error) => failures.push(format!("{case}: log SVG failed: {error}")),
            }
        }

        let aggregate_deadline = std::time::Instant::now() + LOG_RENDERING_AGGREGATE_TIMEOUT;
        for case in [
            "finite-extremes-line",
            "full-domain-semantics",
            "explicit-adjacent-high",
            "auto-adjacent-high",
        ] {
            record_bounded_log_rendering_case(&mut failures, case, aggregate_deadline);
        }

        for orientation in [BarOrientation::Vertical, BarOrientation::Horizontal] {
            let opts = ChartOptions {
                chart_type: ChartType::Bar,
                x_column: Some("category".into()),
                y_column: Some("value".into()),
                format: ChartFormat::Png,
                width: 400,
                height: 300,
                y_range: Some([1.0, 1000.0]),
                ..ChartOptions::default()
            };
            match render_chart_with_presentation(&bar_rows, &opts, log_presentation(orientation)) {
                Ok(result)
                    if result.mime_type == "image/png"
                        && result
                            .bytes
                            .starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) => {}
                Ok(result) => failures.push(format!(
                    "{orientation:?} log PNG has wrong MIME/magic: {}, {:?}",
                    result.mime_type,
                    result.bytes.get(..8)
                )),
                Err(error) => failures.push(format!("{orientation:?} log PNG failed: {error}")),
            }
        }

        for (case, chart_type, rows, range) in [
            (
                "zero",
                ChartType::Bar,
                vec![serde_json::json!({"category": "A", "value": 0.0})],
                None,
            ),
            (
                "negative",
                ChartType::Line,
                vec![serde_json::json!({"category": 1, "value": -1.0})],
                None,
            ),
            (
                "mixed sign",
                ChartType::Scatter,
                vec![
                    serde_json::json!({"category": 1, "value": -1.0}),
                    serde_json::json!({"category": 2, "value": 1.0}),
                ],
                None,
            ),
            (
                "histogram",
                ChartType::Histogram,
                vec![serde_json::json!({"category": 1.0, "value": 10.0})],
                None,
            ),
            (
                "range excludes value",
                ChartType::Bar,
                bar_rows.clone(),
                Some([20.0, 200.0]),
            ),
            (
                "non-positive range",
                ChartType::Bar,
                bar_rows.clone(),
                Some([0.0, 200.0]),
            ),
            (
                "reversed range",
                ChartType::Bar,
                bar_rows.clone(),
                Some([200.0, 1.0]),
            ),
        ] {
            let opts = ChartOptions {
                chart_type,
                x_column: Some("category".into()),
                y_column: Some("value".into()),
                format: ChartFormat::Svg,
                y_range: range,
                ..ChartOptions::default()
            };
            match render_chart_with_presentation(
                &rows,
                &opts,
                log_presentation(BarOrientation::Vertical),
            ) {
                Err(error) if error.code == ErrorCode::InvalidArgument => {}
                Err(error) => failures.push(format!(
                    "{case}: expected InvalidArgument, got {:?}: {}",
                    error.code, error.message
                )),
                Ok(_) => failures.push(format!("{case}: invalid log chart rendered successfully")),
            }
        }

        assert!(
            failures.is_empty(),
            "log rendering failures:\n{}",
            failures.join("\n")
        );
    }
}
