// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Wire format codes and result-column metadata shared by the query and prepared-statement paths.

use super::error::{Error, Result};
use crate::types::Oid;
use std::borrow::Cow;

/// Wire format for a *bound parameter* in the `Bind` message.
///
/// Distinct from [`ColumnFormat`], which describes *result* columns and
/// carries a third `HyperBinary` variant. Parameters only ever travel as
/// PostgreSQL text or PostgreSQL binary — Hyper has no `HyperBinary` input
/// decoder for bound parameters.
///
/// Binary is the default and the fast path. Text exists because Hyper has
/// no PG-binary *input* function for a couple of types:
///
/// - **scaled `NUMERIC`** — a binary NUMERIC whose `dscale` exceeds the
///   parameter's resolved scale is rejected with SQLSTATE `0A000`
///   ("cannot handle truncation when reading numerics").
/// - **`geography`** — rejected with `42883`, "no pg binary input function
///   available for type geography".
///
/// The `Bind` message carries a *per-parameter* format-code array, so a
/// single statement can mix both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParamFormat {
    /// PostgreSQL text format (format code `0`) — the value's SQL literal
    /// representation, sent as UTF-8 without surrounding quotes.
    Text,
    /// Standard PostgreSQL binary format (format code `1`, big-endian).
    #[default]
    Binary,
}

impl ParamFormat {
    /// Wire format code for [`ParamFormat::Binary`].
    pub const BINARY_CODE: i16 = 1;
    /// Wire format code for [`ParamFormat::Text`].
    pub const TEXT_CODE: i16 = 0;

    /// Returns the wire protocol format code (`0` = text, `1` = binary).
    #[must_use]
    pub fn to_code(self) -> i16 {
        match self {
            ParamFormat::Text => Self::TEXT_CODE,
            ParamFormat::Binary => Self::BINARY_CODE,
        }
    }

    /// Returns true if this is the binary format.
    #[must_use]
    pub fn is_binary(self) -> bool {
        matches!(self, ParamFormat::Binary)
    }
}

/// A single binary format code, which `Bind` broadcasts to every parameter.
const BROADCAST_BINARY: &[i16] = &[ParamFormat::BINARY_CODE];

/// Builds the `Bind` parameter-format-code array for `param_count`
/// parameters described by `formats`.
///
/// **An empty `formats` means "every parameter is binary"** — the crate-wide
/// shorthand, and deliberately *not* the wire meaning. The PostgreSQL
/// protocol reads a zero-length format array as "every parameter is text",
/// so emitting `formats` straight through would reinterpret every binary
/// parameter as text: silent corruption rather than an error. This is the
/// single place that translation happens, which is why the function needs
/// `param_count` — it is the only way to tell "no parameters" (where a
/// zero-length array is correct) from "no format overrides".
///
/// The protocol also lets the array hold `1` code that applies to *all*
/// parameters. The all-binary case — every parameterized query that doesn't
/// bind a scaled `NUMERIC` or a `geography` — therefore ships one code
/// instead of `n` and borrows a `const`, so the hot path performs no
/// allocation at all.
///
/// # Errors
///
/// Returns a protocol error if `formats` is non-empty and its length differs
/// from `param_count`. Broadcasting a mismatched array would bind parameters
/// under a format the caller never chose.
pub(crate) fn bind_format_codes(
    formats: &[ParamFormat],
    param_count: usize,
) -> Result<Cow<'static, [i16]>> {
    if !formats.is_empty() && formats.len() != param_count {
        return Err(Error::protocol(format!(
            "parameter format count ({}) does not match parameter count ({param_count})",
            formats.len()
        )));
    }
    if param_count == 0 {
        return Ok(Cow::Borrowed(&[]));
    }
    if formats.is_empty() || formats.iter().copied().all(ParamFormat::is_binary) {
        return Ok(Cow::Borrowed(BROADCAST_BINARY));
    }
    Ok(Cow::Owned(
        formats.iter().copied().map(ParamFormat::to_code).collect(),
    ))
}

/// Format code for column data.
///
/// This indicates how data values are encoded in the wire protocol.
/// The format affects how values are serialized/deserialized and can
/// significantly impact performance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnFormat {
    /// PostgreSQL text format (format code `0`, UTF-8).
    ///
    /// Values are sent as UTF-8 strings. Slower but human-readable.
    /// Use for debugging or when compatibility with text-based tools is needed.
    #[default]
    Text,
    /// Standard `PostgreSQL` binary format.
    ///
    /// Uses `PostgreSQL`'s standard binary encoding (`BigEndian` for most types).
    /// Compatible with standard `PostgreSQL` clients.
    Binary,
    /// Hyper-specific binary format (little-endian, optimized).
    ///
    /// Uses Hyper's optimized binary format where all multi-byte values are
    /// **little-endian** (x86/ARM-LE native byte order), avoiding byte-swapping
    /// on modern hardware. This contrasts with standard `PostgreSQL` binary format
    /// which uses **big-endian** (network byte order).
    ///
    /// Additional differences from `PostgreSQL` binary:
    /// - No per-row field count prefix (rows are implicitly framed)
    /// - NULL is a 1-byte indicator on nullable columns only (vs 4-byte `-1` length)
    /// - Fixed-size types have no length prefix (vs 4-byte length in PG binary)
    ///
    /// This is the fastest format and is used by default for `query_fast()`,
    /// `query_streaming()`, and the COPY bulk insertion path.
    HyperBinary,
}

impl ColumnFormat {
    /// Creates a `ColumnFormat` from the wire protocol format code.
    ///
    /// Format codes: 0 = Text, 1 = Binary, 2 = `HyperBinary`
    #[must_use]
    pub fn from_code(code: i16) -> Self {
        match code {
            0 => ColumnFormat::Text,
            1 => ColumnFormat::Binary,
            2 => ColumnFormat::HyperBinary,
            _ => ColumnFormat::Text, // Default to text for unknown codes
        }
    }

    /// Returns the wire protocol format code.
    #[must_use]
    pub fn to_code(self) -> i16 {
        match self {
            ColumnFormat::Text => 0,
            ColumnFormat::Binary => 1,
            ColumnFormat::HyperBinary => 2,
        }
    }

    /// Returns true if this is a binary format (Binary or `HyperBinary`).
    #[must_use]
    pub fn is_binary(self) -> bool {
        matches!(self, ColumnFormat::Binary | ColumnFormat::HyperBinary)
    }
}

/// Metadata about a column in a result set.
///
/// `Column` carries the three pieces of information the wire protocol's
/// `RowDescription` message provides for each result field: the column
/// name, its type OID, and the type modifier that encodes width-specific
/// parameters like `NUMERIC(precision, scale)` or `VARCHAR(n)`.
///
/// The type modifier is essential for decoding types whose wire format
/// depends on declared precision/scale (e.g. `NUMERIC`, where precision
/// ≤ 18 uses an 8-byte `i64` wire form and precision > 18 uses a
/// 16-byte `i128` wire form — and in both cases the scale needed to
/// interpret the unscaled integer value lives only in the type
/// modifier). Upper layers construct a `SqlType` from OID + modifier
/// via [`crate::types::SqlType::from_oid_and_modifier`] — using
/// [`crate::types::SqlType::from_oid`] alone silently drops precision
/// and scale, causing decoders to default to scale = 0 and corrupt
/// fractional values.
#[derive(Debug, Clone)]
pub struct Column {
    pub(crate) name: String,
    pub(crate) type_oid: Oid,
    /// PostgreSQL-style type modifier. For NUMERIC columns the encoding
    /// is `((precision << 16) | scale) + 4`; for VARCHAR it's
    /// `length + 4`; for most other types the server sends `-1`
    /// (no modifier). Parse with
    /// [`SqlType::from_oid_and_modifier`](crate::types::SqlType::from_oid_and_modifier).
    pub(crate) type_modifier: i32,
    pub(crate) format: ColumnFormat,
}

impl Column {
    /// Creates a new Column.
    #[inline]
    pub(crate) fn new(
        name: String,
        type_oid: Oid,
        type_modifier: i32,
        format: ColumnFormat,
    ) -> Self {
        Column {
            name,
            type_oid,
            type_modifier,
            format,
        }
    }

    /// Returns the column name.
    #[inline]
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the column type OID.
    #[inline]
    #[must_use]
    pub fn type_oid(&self) -> Oid {
        self.type_oid
    }

    /// Returns the column's type modifier (PostgreSQL-style `atttypmod`).
    ///
    /// For `NUMERIC` columns this encodes precision and scale and is
    /// required for correct decode (see
    /// [`crate::types::SqlType::from_oid_and_modifier`]). For most other
    /// types the server sends `-1` to indicate "no modifier".
    #[inline]
    #[must_use]
    pub fn type_modifier(&self) -> i32 {
        self.type_modifier
    }

    /// Returns the data format for this column.
    #[inline]
    #[must_use]
    pub fn format(&self) -> ColumnFormat {
        self.format
    }
}

#[cfg(test)]
mod tests {
    use super::{ParamFormat, bind_format_codes};

    #[test]
    fn all_binary_collapses_to_one_broadcast_code() {
        // PG applies a lone format code to every parameter, so N binary
        // parameters need exactly one code on the wire.
        for n in 1..=8 {
            let formats = vec![ParamFormat::Binary; n];
            assert_eq!(
                &*bind_format_codes(&formats, n).unwrap(),
                &[1_i16],
                "{n} binary params should ship one broadcast code"
            );
        }
    }

    #[test]
    fn empty_formats_mean_all_binary_not_all_text() {
        // Regression: the helper used to hand an empty slice straight back,
        // and PG reads a zero-length format array as "every parameter is
        // text". Every caller in the crate means "all binary" by `&[]`, so
        // passing it through silently reinterpreted binary bytes as text.
        for n in 1..=8 {
            assert_eq!(
                &*bind_format_codes(&[], n).unwrap(),
                &[1_i16],
                "empty formats with {n} params must broadcast binary"
            );
        }
    }

    #[test]
    fn zero_parameters_send_no_format_codes() {
        // A broadcast code with no parameters would be a malformed Bind.
        // This is the *only* case where a zero-length array is correct.
        assert!(bind_format_codes(&[], 0).unwrap().is_empty());
    }

    #[test]
    fn format_count_must_match_parameter_count() {
        // Silently broadcasting a one-element array over three parameters, or
        // truncating a three-element array to two parameters, binds values
        // under a format the caller never chose.
        for (formats, param_count) in [
            (vec![ParamFormat::Text], 3),
            (vec![ParamFormat::Text, ParamFormat::Binary], 3),
            (vec![ParamFormat::Binary; 3], 2),
            (vec![ParamFormat::Binary], 0),
        ] {
            let err = bind_format_codes(&formats, param_count)
                .expect_err("length mismatch must be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains("does not match parameter count"),
                "unexpected error for {}/{param_count}: {msg}",
                formats.len()
            );
        }
    }

    #[test]
    fn mixed_formats_expand_to_one_code_per_parameter() {
        assert_eq!(
            &*bind_format_codes(&[ParamFormat::Binary, ParamFormat::Text], 2).unwrap(),
            &[1_i16, 0]
        );
        assert_eq!(
            &*bind_format_codes(&[ParamFormat::Text, ParamFormat::Binary], 2).unwrap(),
            &[0_i16, 1]
        );
        // All-text is still a per-parameter array, never an empty one — an
        // empty array means "all binary" to this helper, and all-text to PG.
        assert_eq!(
            &*bind_format_codes(&[ParamFormat::Text, ParamFormat::Text], 2).unwrap(),
            &[0_i16, 0]
        );
    }

    #[test]
    fn param_format_codes_match_the_protocol() {
        assert_eq!(ParamFormat::Text.to_code(), 0);
        assert_eq!(ParamFormat::Binary.to_code(), 1);
        assert!(ParamFormat::Binary.is_binary());
        assert!(!ParamFormat::Text.is_binary());
        assert_eq!(ParamFormat::default(), ParamFormat::Binary);
    }
}
