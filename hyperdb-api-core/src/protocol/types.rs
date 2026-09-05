// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Type conversion utilities for `HyperBinary` format.
//!
//! This module provides functions for converting between Rust types and
//! Hyper's **`LittleEndian`** binary format. These conversions are used when
//! reading `DataRow` payloads from query results.
//!
//! Each type has a paired `*_to_hyper_binary` / `*_from_hyper_binary` function.
//! Fixed-size types require exact buffer lengths; variable-length types
//! (text, bytea) use a 4-byte `LittleEndian` length prefix followed by the data.

use bytes::BytesMut;
use std::fmt;

/// Narrows a `usize` length to the `HyperBinary` 4-byte little-endian length
/// prefix. Panics on overflow; values >`u32::MAX` bytes are a programming
/// error per the `HyperBinary` format contract.
#[inline]
fn hb_len(n: usize) -> u32 {
    u32::try_from(n).expect("HyperBinary variable-length value exceeds u32::MAX")
}

/// Error type for `HyperBinary` parsing/decoding operations.
///
/// Used when converting from `HyperBinary` format (`LittleEndian`) to Rust types.
/// All numeric values in `HyperBinary` are `LittleEndian`, unlike the message framing
/// which uses `BigEndian`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Buffer has wrong size for the expected fixed-size type.
    ///
    /// Fixed-size types (bool, i16, i32, i64, f32, f64) require exact buffer sizes.
    InvalidLength {
        /// Name of the type being parsed (e.g., "i32", "f64").
        type_name: &'static str,
        /// Expected buffer size in bytes.
        expected: usize,
        /// Actual buffer size in bytes.
        actual: usize,
    },
    /// Buffer is too short for variable-length data.
    ///
    /// Used for types like text and bytea that have length prefixes.
    BufferTooShort {
        /// Context describing what was being parsed (e.g., "text length", "text data").
        context: &'static str,
    },
    /// Invalid UTF-8 in text data.
    ///
    /// Text values in `HyperBinary` must be valid UTF-8 strings.
    InvalidUtf8(std::str::Utf8Error),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::InvalidLength {
                type_name,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "invalid buffer size for {type_name}: expected {expected}, got {actual}"
                )
            }
            ParseError::BufferTooShort { context } => {
                write!(f, "buffer too short for {context}")
            }
            ParseError::InvalidUtf8(e) => {
                write!(f, "invalid UTF-8: {e}")
            }
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ParseError::InvalidUtf8(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::str::Utf8Error> for ParseError {
    fn from(e: std::str::Utf8Error) -> Self {
        ParseError::InvalidUtf8(e)
    }
}

/// Serializes a boolean value to `HyperBinary` format.
///
/// Encodes `true` as 1, `false` as 0 (single byte).
///
/// # Arguments
///
/// * `v` - Boolean value to serialize
/// * `buf` - Buffer to append the encoded value to
#[inline]
pub fn bool_to_hyper_binary(v: bool, buf: &mut BytesMut) {
    buf.extend_from_slice(&[u8::from(v)]);
}

/// Deserializes a boolean value from `HyperBinary` format.
///
/// Decodes 0 as `false`, any non-zero value as `true`.
///
/// # Errors
///
/// Returns `ParseError::InvalidLength` if the buffer is not exactly 1 byte.
#[inline]
pub fn bool_from_hyper_binary(buf: &[u8]) -> Result<bool, ParseError> {
    if buf.len() != 1 {
        return Err(ParseError::InvalidLength {
            type_name: "bool",
            expected: 1,
            actual: buf.len(),
        });
    }
    Ok(buf[0] != 0)
}

/// Serializes an i16 value to `HyperBinary` format (`LittleEndian`).
///
/// Writes 2 bytes in `LittleEndian` byte order.
///
/// # Arguments
///
/// * `v` - i16 value to serialize
/// * `buf` - Buffer to append the encoded value to
#[inline]
pub fn i16_to_hyper_binary(v: i16, buf: &mut BytesMut) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Deserializes an i16 value from `HyperBinary` format (`LittleEndian`).
///
/// Reads 2 bytes in `LittleEndian` byte order.
///
/// # Errors
///
/// Returns `ParseError::InvalidLength` if the buffer is not exactly 2 bytes.
#[inline]
pub fn i16_from_hyper_binary(buf: &[u8]) -> Result<i16, ParseError> {
    let bytes = buf.try_into().map_err(|_| ParseError::InvalidLength {
        type_name: "i16",
        expected: 2,
        actual: buf.len(),
    })?;
    Ok(i16::from_le_bytes(bytes))
}

/// Serializes an i32 value to `HyperBinary` format (`LittleEndian`).
///
/// Writes 4 bytes in `LittleEndian` byte order.
///
/// # Arguments
///
/// * `v` - i32 value to serialize
/// * `buf` - Buffer to append the encoded value to
#[inline]
pub fn i32_to_hyper_binary(v: i32, buf: &mut BytesMut) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Deserializes an i32 value from `HyperBinary` format (`LittleEndian`).
///
/// Reads 4 bytes in `LittleEndian` byte order.
///
/// # Errors
///
/// Returns `ParseError::InvalidLength` if the buffer is not exactly 4 bytes.
#[inline]
pub fn i32_from_hyper_binary(buf: &[u8]) -> Result<i32, ParseError> {
    let bytes = buf.try_into().map_err(|_| ParseError::InvalidLength {
        type_name: "i32",
        expected: 4,
        actual: buf.len(),
    })?;
    Ok(i32::from_le_bytes(bytes))
}

/// Serializes an i64 value to `HyperBinary` format (`LittleEndian`).
///
/// Writes 8 bytes in `LittleEndian` byte order.
///
/// # Arguments
///
/// * `v` - i64 value to serialize
/// * `buf` - Buffer to append the encoded value to
#[inline]
pub fn i64_to_hyper_binary(v: i64, buf: &mut BytesMut) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Deserializes an i64 value from `HyperBinary` format (`LittleEndian`).
///
/// Reads 8 bytes in `LittleEndian` byte order.
///
/// # Errors
///
/// Returns `ParseError::InvalidLength` if the buffer is not exactly 8 bytes.
#[inline]
pub fn i64_from_hyper_binary(buf: &[u8]) -> Result<i64, ParseError> {
    let bytes = buf.try_into().map_err(|_| ParseError::InvalidLength {
        type_name: "i64",
        expected: 8,
        actual: buf.len(),
    })?;
    Ok(i64::from_le_bytes(bytes))
}

/// Serializes an f32 value to `HyperBinary` format (`LittleEndian`).
///
/// Writes 4 bytes in `LittleEndian` byte order (IEEE 754 single precision).
///
/// # Arguments
///
/// * `v` - f32 value to serialize
/// * `buf` - Buffer to append the encoded value to
#[inline]
pub fn f32_to_hyper_binary(v: f32, buf: &mut BytesMut) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Deserializes an f32 value from `HyperBinary` format (`LittleEndian`).
///
/// Reads 4 bytes in `LittleEndian` byte order (IEEE 754 single precision).
///
/// # Errors
///
/// Returns `ParseError::InvalidLength` if the buffer is not exactly 4 bytes.
#[inline]
pub fn f32_from_hyper_binary(buf: &[u8]) -> Result<f32, ParseError> {
    let bytes = buf.try_into().map_err(|_| ParseError::InvalidLength {
        type_name: "f32",
        expected: 4,
        actual: buf.len(),
    })?;
    Ok(f32::from_le_bytes(bytes))
}

/// Serializes an f64 value to `HyperBinary` format (`LittleEndian`).
///
/// Writes 8 bytes in `LittleEndian` byte order (IEEE 754 double precision).
///
/// # Arguments
///
/// * `v` - f64 value to serialize
/// * `buf` - Buffer to append the encoded value to
#[inline]
pub fn f64_to_hyper_binary(v: f64, buf: &mut BytesMut) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Deserializes an f64 value from `HyperBinary` format (`LittleEndian`).
///
/// Reads 8 bytes in `LittleEndian` byte order (IEEE 754 double precision).
///
/// # Errors
///
/// Returns `ParseError::InvalidLength` if the buffer is not exactly 8 bytes.
#[inline]
pub fn f64_from_hyper_binary(buf: &[u8]) -> Result<f64, ParseError> {
    let bytes = buf.try_into().map_err(|_| ParseError::InvalidLength {
        type_name: "f64",
        expected: 8,
        actual: buf.len(),
    })?;
    Ok(f64::from_le_bytes(bytes))
}

/// Serializes a text value to `HyperBinary` format.
///
/// Variable-length strings are stored as: [length: 4 bytes LE][data: N bytes]
///
/// The length is the number of bytes (not UTF-8 code points).
///
/// # Arguments
///
/// * `v` - String to serialize (must be valid UTF-8)
/// * `buf` - Buffer to append the encoded value to
#[inline]
pub fn text_to_hyper_binary(v: &str, buf: &mut BytesMut) {
    buf.extend_from_slice(&hb_len(v.len()).to_le_bytes());
    buf.extend_from_slice(v.as_bytes());
}

/// Deserializes a text value from `HyperBinary` format.
///
/// Reads a 4-byte length prefix (`LittleEndian`) followed by the string data.
/// The string must be valid UTF-8.
///
/// # Errors
///
/// Returns `ParseError::BufferTooShort` if the buffer is too short for the length field
/// or the declared data length. Returns `ParseError::InvalidUtf8` if the data is not valid UTF-8.
#[inline]
pub fn text_from_hyper_binary(buf: &[u8]) -> Result<&str, ParseError> {
    let (len_bytes, rest) = buf
        .split_first_chunk::<4>()
        .ok_or(ParseError::BufferTooShort {
            context: "text length",
        })?;
    let len = u32::from_le_bytes(*len_bytes) as usize;
    let data = rest.get(..len).ok_or(ParseError::BufferTooShort {
        context: "text data",
    })?;
    Ok(std::str::from_utf8(data)?)
}

/// Serializes a bytea value to `HyperBinary` format.
///
/// Variable-length binary data is stored as: [length: 4 bytes LE][data: N bytes]
///
/// # Arguments
///
/// * `v` - Byte slice to serialize
/// * `buf` - Buffer to append the encoded value to
#[inline]
pub fn bytea_to_hyper_binary(v: &[u8], buf: &mut BytesMut) {
    buf.extend_from_slice(&hb_len(v.len()).to_le_bytes());
    buf.extend_from_slice(v);
}

/// Deserializes a bytea value from `HyperBinary` format.
///
/// Reads a 4-byte length prefix (`LittleEndian`) followed by the binary data.
///
/// # Errors
///
/// Returns `ParseError::BufferTooShort` if the buffer is too short for the length field
/// or the declared data length.
#[inline]
pub fn bytea_from_hyper_binary(buf: &[u8]) -> Result<&[u8], ParseError> {
    let (len_bytes, rest) = buf
        .split_first_chunk::<4>()
        .ok_or(ParseError::BufferTooShort {
            context: "bytea length",
        })?;
    let len = u32::from_le_bytes(*len_bytes) as usize;
    rest.get(..len).ok_or(ParseError::BufferTooShort {
        context: "bytea data",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A declared length that no buffer can satisfy must be rejected, not
    /// trusted. `u32::MAX` is the interesting value: the previous
    /// implementation computed `4 + len`, which overflows `usize` on a 32-bit
    /// target and wraps to a small number, making the bounds check pass and
    /// the subsequent slice index panic. `split_first_chunk::<4>` followed by
    /// `slice::get` removes the arithmetic entirely, so there is nothing left
    /// to overflow.
    #[test]
    fn oversized_declared_length_is_rejected() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        buf.extend_from_slice(b"only a few bytes");

        assert!(matches!(
            text_from_hyper_binary(&buf),
            Err(ParseError::BufferTooShort {
                context: "text data"
            })
        ));
        assert!(matches!(
            bytea_from_hyper_binary(&buf),
            Err(ParseError::BufferTooShort {
                context: "bytea data"
            })
        ));
    }

    /// A buffer too short to even hold the 4-byte length prefix.
    #[test]
    fn truncated_length_prefix_is_rejected() {
        for short in [&[][..], &[0][..], &[0, 0][..], &[0, 0, 0][..]] {
            assert!(matches!(
                text_from_hyper_binary(short),
                Err(ParseError::BufferTooShort {
                    context: "text length"
                })
            ));
        }
    }

    /// The fixed-width readers require an exact length; `try_into` enforces it.
    #[test]
    fn fixed_width_readers_require_exact_length() {
        assert!(i16_from_hyper_binary(&[0]).is_err());
        assert!(i16_from_hyper_binary(&[0, 0, 0]).is_err());
        assert!(i32_from_hyper_binary(&[0, 0, 0]).is_err());
        assert!(i64_from_hyper_binary(&[0; 7]).is_err());
        assert!(f32_from_hyper_binary(&[0; 5]).is_err());
        assert!(f64_from_hyper_binary(&[0; 9]).is_err());

        assert_eq!(i16_from_hyper_binary(&[0, 0]).unwrap(), 0);
        assert_eq!(i32_from_hyper_binary(&[0; 4]).unwrap(), 0);
        assert_eq!(i64_from_hyper_binary(&[0; 8]).unwrap(), 0);
    }

    /// A zero-length payload is valid and distinct from a truncated buffer.
    #[test]
    fn zero_length_payload_is_valid() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(text_from_hyper_binary(&buf).unwrap(), "");
        assert_eq!(bytea_from_hyper_binary(&buf).unwrap(), b"");
    }

    #[test]
    fn test_i32_roundtrip() {
        let mut buf = BytesMut::new();
        i32_to_hyper_binary(12345678, &mut buf);
        assert_eq!(i32_from_hyper_binary(&buf).unwrap(), 12345678);
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "bit-for-bit round-trip through fixed-width binary encoding; epsilon compare would mask a decode regression"
    )]
    fn test_f64_roundtrip() {
        let mut buf = BytesMut::new();
        f64_to_hyper_binary(std::f64::consts::PI, &mut buf);
        assert_eq!(f64_from_hyper_binary(&buf).unwrap(), std::f64::consts::PI);
    }

    #[test]
    fn test_text_roundtrip() {
        let mut buf = BytesMut::new();
        text_to_hyper_binary("hello world", &mut buf);
        assert_eq!(text_from_hyper_binary(&buf).unwrap(), "hello world");
    }
}
