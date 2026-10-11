// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! SQL type definitions for Hyper.
//!
//! This module provides type definitions that map to Hyper's SQL types:
//!
//! - [`SqlType`] - SQL type enumeration with OID and modifier support
//! - [`Nullability`] - Whether a column allows NULL values
//!
//! # Example
//!
//! ```
//! use hyperdb_api_core::types::{SqlType, Nullability};
//!
//! // Create various SQL types
//! let int_type = SqlType::int();
//! let text_type = SqlType::text();
//! let numeric_type = SqlType::numeric(18, 2);
//!
//! // Check nullability
//! let nullable = Nullability::Nullable;
//! assert!(nullable.is_nullable());
//! ```

use std::fmt;

use super::oid::oids;
use super::special::Numeric;

// =============================================================================
// SqlType
// =============================================================================

/// SQL type tags that identify the type of a column.
///
/// Each variant corresponds to a Hyper SQL type and has an associated internal OID.
///
/// The enum and its struct variants (`Numeric`, `Varchar`, `Char`) are
/// `#[non_exhaustive]`: match with a wildcard arm, end field patterns with
/// `..`, and build values with the constructors (for example
/// [`SqlType::numeric`]).
///
/// # Example
///
/// ```
/// use hyperdb_api_core::types::SqlType;
///
/// let int_type = SqlType::int();
/// assert_eq!(int_type.to_string(), "INTEGER");
///
/// let numeric_type = SqlType::numeric(18, 2);
/// assert_eq!(numeric_type.to_string(), "NUMERIC(18, 2)");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SqlType {
    /// Boolean type.
    Bool,
    /// 64-bit integer (BIGINT).
    BigInt,
    /// 16-bit integer (SMALLINT).
    SmallInt,
    /// 32-bit integer (INT).
    #[default]
    Int,
    /// Numeric/decimal type with precision and scale.
    #[non_exhaustive]
    Numeric {
        /// Total number of digits (1-38).
        precision: u32,
        /// Number of digits after decimal point (0-precision).
        scale: u32,
    },
    /// Double precision floating point.
    Double,
    /// Single precision floating point.
    Float,
    /// Object identifier.
    Oid,
    /// Binary data (BYTEA).
    ByteA,
    /// Variable-length text.
    Text,
    /// Variable-length character string with max length.
    #[non_exhaustive]
    Varchar {
        /// Maximum length, or None for unlimited.
        max_length: Option<u32>,
    },
    /// Fixed-length character string.
    #[non_exhaustive]
    Char {
        /// Fixed length.
        length: u32,
    },
    /// JSON data.
    Json,
    /// Date (year, month, day).
    Date,
    /// Time interval.
    Interval,
    /// Time of day.
    Time,
    /// Timestamp without timezone.
    Timestamp,
    /// Timestamp with timezone.
    TimestampTz,
    /// Geography/geometric data.
    Geography,
    /// Unsupported type.
    Unsupported,
}

impl SqlType {
    /// Creates a Bool type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::bool();
    /// assert_eq!(t.to_string(), "BOOLEAN");
    /// ```
    pub fn bool() -> Self {
        SqlType::Bool
    }

    /// Creates a BigInt type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::big_int();
    /// assert_eq!(t.to_string(), "BIGINT");
    /// ```
    pub fn big_int() -> Self {
        SqlType::BigInt
    }

    /// Creates a SmallInt type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::small_int();
    /// assert_eq!(t.to_string(), "SMALLINT");
    /// ```
    pub fn small_int() -> Self {
        SqlType::SmallInt
    }

    /// Creates an Int type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::int();
    /// assert_eq!(t.to_string(), "INTEGER");
    /// ```
    pub fn int() -> Self {
        SqlType::Int
    }

    /// Creates a Numeric type with the given precision and scale.
    ///
    /// # Arguments
    ///
    /// * `precision` - Total number of digits (1-38)
    /// * `scale` - Number of digits after decimal point (0-precision)
    ///
    /// # Panics
    ///
    /// Panics if `precision` is not in `1..=38` or `scale > precision`.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::numeric(18, 2);
    /// assert_eq!(t.to_string(), "NUMERIC(18, 2)");
    /// ```
    pub fn numeric(precision: u32, scale: u32) -> Self {
        assert!(
            precision >= 1 && precision <= u32::from(Numeric::MAX_PRECISION),
            "NUMERIC precision must be between 1 and {}",
            Numeric::MAX_PRECISION
        );
        assert!(
            scale <= precision,
            "NUMERIC scale ({scale}) cannot exceed precision ({precision})"
        );
        SqlType::Numeric { precision, scale }
    }

    /// Creates a Numeric type, or `None` if `precision` is outside
    /// `1..=38` or `scale` exceeds `precision`.
    ///
    /// The non-panicking counterpart of [`SqlType::numeric`], for values that
    /// come from outside the program (for example an Arrow `Decimal256`
    /// schema).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// assert_eq!(SqlType::try_numeric(18, 2), Some(SqlType::numeric(18, 2)));
    /// assert_eq!(SqlType::try_numeric(76, 2), None);
    /// ```
    pub fn try_numeric(precision: u32, scale: u32) -> Option<Self> {
        (precision >= 1 && precision <= u32::from(Numeric::MAX_PRECISION) && scale <= precision)
            .then_some(SqlType::Numeric { precision, scale })
    }

    /// Creates a Double type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::double();
    /// assert_eq!(t.to_string(), "DOUBLE PRECISION");
    /// ```
    pub fn double() -> Self {
        SqlType::Double
    }

    /// Creates a Float type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::float();
    /// assert_eq!(t.to_string(), "REAL");
    /// ```
    pub fn float() -> Self {
        SqlType::Float
    }

    /// Creates an Oid type.
    pub fn oid() -> Self {
        SqlType::Oid
    }

    /// Creates a Text type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::text();
    /// assert_eq!(t.to_string(), "TEXT");
    /// ```
    pub fn text() -> Self {
        SqlType::Text
    }

    /// Creates a Varchar type with optional max length.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::varchar(Some(255));
    /// assert_eq!(t.to_string(), "VARCHAR(255)");
    /// ```
    pub fn varchar(max_length: Option<u32>) -> Self {
        SqlType::Varchar { max_length }
    }

    /// Creates a Char type with the given length.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::char(10);
    /// assert_eq!(t.to_string(), "CHAR(10)");
    /// ```
    pub fn char(length: u32) -> Self {
        SqlType::Char { length }
    }

    /// Creates a Date type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::date();
    /// assert_eq!(t.to_string(), "DATE");
    /// ```
    pub fn date() -> Self {
        SqlType::Date
    }

    /// Creates a Time type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::time();
    /// assert_eq!(t.to_string(), "TIME");
    /// ```
    pub fn time() -> Self {
        SqlType::Time
    }

    /// Creates a Timestamp type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::timestamp();
    /// assert_eq!(t.to_string(), "TIMESTAMP");
    /// ```
    pub fn timestamp() -> Self {
        SqlType::Timestamp
    }

    /// Creates a TimestampTz type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::timestamp_tz();
    /// assert_eq!(t.to_string(), "TIMESTAMPTZ");
    /// ```
    pub fn timestamp_tz() -> Self {
        SqlType::TimestampTz
    }

    /// Creates an Interval type.
    pub fn interval() -> Self {
        SqlType::Interval
    }

    /// Creates a ByteA (binary) type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::bytes();
    /// assert_eq!(t.to_string(), "BYTEA");
    /// ```
    pub fn bytes() -> Self {
        SqlType::ByteA
    }

    /// Creates a Json type.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::json();
    /// assert_eq!(t.to_string(), "JSON");
    /// ```
    pub fn json() -> Self {
        SqlType::Json
    }

    /// Creates a Geography type.
    ///
    /// Note: `tabgeography()` is the preferred method name to match C++ and Java APIs.
    pub fn geography() -> Self {
        SqlType::Geography
    }

    /// Creates a TabGeography type (Tableau geography).
    ///
    /// This is the preferred method for creating geography columns, matching the
    /// C++ and Java API naming conventions.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::tabgeography();
    /// assert_eq!(t.to_string(), "GEOGRAPHY");
    /// ```
    pub fn tabgeography() -> Self {
        SqlType::Geography
    }

    /// Returns the internal OID for this SQL type.
    ///
    /// The internal OID is used by Hyper's protocol layer for type identification.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::int();
    /// assert_eq!(t.internal_oid(), 23);
    /// ```
    pub fn internal_oid(&self) -> u32 {
        match self {
            SqlType::Bool => oids::BOOL.0,
            SqlType::BigInt => oids::BIG_INT.0,
            SqlType::SmallInt => oids::SMALL_INT.0,
            SqlType::Int => oids::INT.0,
            SqlType::Numeric { .. } => oids::NUMERIC.0,
            SqlType::Double => oids::DOUBLE.0,
            SqlType::Float => oids::FLOAT.0,
            SqlType::Oid => oids::OID.0,
            SqlType::ByteA => oids::BYTE_A.0,
            SqlType::Text => oids::TEXT.0,
            SqlType::Varchar { .. } => oids::VARCHAR.0,
            SqlType::Char { length } => {
                if *length == 1 {
                    oids::CHAR1.0
                } else {
                    oids::CHAR.0
                }
            }
            SqlType::Json => oids::JSON.0,
            SqlType::Date => oids::DATE.0,
            SqlType::Interval => oids::INTERVAL.0,
            SqlType::Time => oids::TIME.0,
            SqlType::Timestamp => oids::TIMESTAMP.0,
            SqlType::TimestampTz => oids::TIMESTAMP_TZ.0,
            SqlType::Geography => oids::GEOGRAPHY.0,
            SqlType::Unsupported => 0,
        }
    }

    /// Returns the precision for Numeric types, or None for other types.
    pub fn precision(&self) -> Option<u32> {
        match self {
            SqlType::Numeric { precision, .. } => Some(*precision),
            _ => None,
        }
    }

    /// Returns the scale for Numeric types, or None for other types.
    pub fn scale(&self) -> Option<u32> {
        match self {
            SqlType::Numeric { scale, .. } => Some(*scale),
            _ => None,
        }
    }

    /// Returns the max length for Varchar/Char types, or None for other types.
    pub fn max_length(&self) -> Option<u32> {
        match self {
            SqlType::Varchar { max_length } => *max_length,
            SqlType::Char { length } => Some(*length),
            _ => None,
        }
    }

    /// Creates a SqlType from an OID.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::SqlType;
    /// let t = SqlType::from_oid(23);
    /// assert_eq!(t, SqlType::Int);
    /// ```
    pub fn from_oid(oid: u32) -> Self {
        match oid {
            x if x == oids::BOOL.0 => SqlType::Bool,
            x if x == oids::BIG_INT.0 => SqlType::BigInt,
            x if x == oids::SMALL_INT.0 => SqlType::SmallInt,
            x if x == oids::INT.0 => SqlType::Int,
            x if x == oids::NUMERIC.0 => SqlType::Numeric {
                precision: 0,
                scale: 0,
            },
            x if x == oids::DOUBLE.0 => SqlType::Double,
            x if x == oids::FLOAT.0 => SqlType::Float,
            x if x == oids::OID.0 => SqlType::Oid,
            x if x == oids::BYTE_A.0 => SqlType::ByteA,
            x if x == oids::TEXT.0 => SqlType::Text,
            x if x == oids::VARCHAR.0 => SqlType::Varchar { max_length: None },
            x if x == oids::CHAR.0 || x == oids::CHAR1.0 => SqlType::Char { length: 1 },
            x if x == oids::JSON.0 => SqlType::Json,
            x if x == oids::DATE.0 => SqlType::Date,
            x if x == oids::INTERVAL.0 => SqlType::Interval,
            x if x == oids::TIME.0 => SqlType::Time,
            x if x == oids::TIMESTAMP.0 => SqlType::Timestamp,
            x if x == oids::TIMESTAMP_TZ.0 => SqlType::TimestampTz,
            x if x == oids::GEOGRAPHY.0 => SqlType::Geography,
            _ => SqlType::Unsupported,
        }
    }

    /// Creates a SqlType from an OID and type modifier.
    ///
    /// Type modifiers encode additional information like precision/scale for numeric
    /// types or max length for varchar types.
    ///
    /// For NUMERIC types, the modifier format is: `((precision << 16) | scale) + 4`.
    /// Valid modifiers are >= 4 (since minimum valid precision is 1 and scale is 0).
    /// Malformed modifiers (values 1-3, or values producing precision > 38 or scale > precision)
    /// are treated as unspecified (precision=0, scale=0).
    ///
    /// # Panics
    ///
    /// Does not panic in practice. The NUMERIC, VARCHAR, and CHAR branches
    /// each convert `modifier - 4` to `u32` via `try_from().expect(...)`
    /// guarded by a `modifier < 4` check, so the subtraction is always
    /// non-negative.
    pub fn from_oid_and_modifier(oid: u32, modifier: i32) -> Self {
        match oid {
            x if x == oids::NUMERIC.0 => {
                // Modifier must be > 4 for valid precision/scale encoding.
                // Values <= 0 indicate unspecified, values 1-3 are malformed.
                if modifier < 4 {
                    SqlType::Numeric {
                        precision: 0,
                        scale: 0,
                    }
                } else {
                    // Modifier format: ((precision << 16) | scale) + 4.
                    // `modifier >= 4` checked above, so `modifier - 4 >= 0`.
                    let mod_val = u32::try_from(modifier - 4)
                        .expect("modifier >= 4 checked above, so (modifier - 4) is non-negative");
                    let precision = mod_val >> 16;
                    let scale = mod_val & 0xFFFF;

                    // Validate bounds using the canonical constant from Numeric
                    if precision > u32::from(Numeric::MAX_PRECISION) || scale > precision {
                        // Malformed modifier: treat as unspecified
                        SqlType::Numeric {
                            precision: 0,
                            scale: 0,
                        }
                    } else {
                        SqlType::Numeric { precision, scale }
                    }
                }
            }
            x if x == oids::VARCHAR.0 => {
                // Modifier must be >= 4 for valid length encoding.
                // Values <= 0 indicate unspecified, values 1-3 are malformed.
                if modifier < 4 {
                    SqlType::Varchar { max_length: None }
                } else {
                    // Modifier format: max_length + 4.
                    // `modifier >= 4` checked above, so `modifier - 4 >= 0`.
                    let max_length = u32::try_from(modifier - 4)
                        .expect("modifier >= 4 checked above, so (modifier - 4) is non-negative");
                    SqlType::Varchar {
                        max_length: Some(max_length),
                    }
                }
            }
            x if x == oids::CHAR.0 => {
                // Modifier must be >= 4 for valid length encoding.
                // Values <= 0 indicate unspecified, values 1-3 are malformed.
                if modifier < 4 {
                    SqlType::Char { length: 1 }
                } else {
                    // `modifier >= 4` checked above, so `modifier - 4 >= 0`.
                    let length = u32::try_from(modifier - 4)
                        .expect("modifier >= 4 checked above, so (modifier - 4) is non-negative");
                    SqlType::Char { length }
                }
            }
            _ => Self::from_oid(oid),
        }
    }

    /// Returns true if this type is a numeric type (can store numbers).
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            SqlType::SmallInt
                | SqlType::Int
                | SqlType::BigInt
                | SqlType::Float
                | SqlType::Double
                | SqlType::Numeric { .. }
        )
    }

    /// Returns true if this type is a string type.
    pub fn is_string(&self) -> bool {
        matches!(
            self,
            SqlType::Text | SqlType::Varchar { .. } | SqlType::Char { .. } | SqlType::Json
        )
    }

    /// Returns true if this type is a temporal type.
    pub fn is_temporal(&self) -> bool {
        matches!(
            self,
            SqlType::Date
                | SqlType::Time
                | SqlType::Timestamp
                | SqlType::TimestampTz
                | SqlType::Interval
        )
    }

    /// Returns true if this is an unsupported type.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, SqlType::Unsupported)
    }
}

impl fmt::Display for SqlType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlType::Bool => write!(f, "BOOLEAN"),
            SqlType::BigInt => write!(f, "BIGINT"),
            SqlType::SmallInt => write!(f, "SMALLINT"),
            SqlType::Int => write!(f, "INTEGER"),
            SqlType::Numeric { precision, scale } => {
                write!(f, "NUMERIC({precision}, {scale})")
            }
            SqlType::Double => write!(f, "DOUBLE PRECISION"),
            SqlType::Float => write!(f, "REAL"),
            SqlType::Oid => write!(f, "OID"),
            SqlType::ByteA => write!(f, "BYTEA"),
            SqlType::Text => write!(f, "TEXT"),
            SqlType::Varchar {
                max_length: Some(n),
            } => write!(f, "VARCHAR({n})"),
            SqlType::Varchar { max_length: None } => write!(f, "VARCHAR"),
            SqlType::Char { length } => write!(f, "CHAR({length})"),
            SqlType::Json => write!(f, "JSON"),
            SqlType::Date => write!(f, "DATE"),
            SqlType::Interval => write!(f, "INTERVAL"),
            SqlType::Time => write!(f, "TIME"),
            SqlType::Timestamp => write!(f, "TIMESTAMP"),
            SqlType::TimestampTz => write!(f, "TIMESTAMPTZ"),
            SqlType::Geography => write!(f, "GEOGRAPHY"),
            SqlType::Unsupported => write!(f, "UNSUPPORTED"),
        }
    }
}

// =============================================================================
// Nullability
// =============================================================================

/// Specifies whether a column allows NULL values.
///
/// # Example
///
/// ```
/// use hyperdb_api_core::types::Nullability;
///
/// let nullable = Nullability::Nullable;
/// let not_null = Nullability::NotNullable;
///
/// assert!(nullable.is_nullable());
/// assert!(!not_null.is_nullable());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Nullability {
    /// The column allows NULL values.
    #[default]
    Nullable,
    /// The column does not allow NULL values.
    NotNullable,
}

impl Nullability {
    /// Returns true if the column is nullable.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::Nullability;
    ///
    /// assert!(Nullability::Nullable.is_nullable());
    /// assert!(!Nullability::NotNullable.is_nullable());
    /// ```
    pub fn is_nullable(&self) -> bool {
        matches!(self, Nullability::Nullable)
    }

    /// Returns true if the column is not nullable.
    pub fn is_not_nullable(&self) -> bool {
        matches!(self, Nullability::NotNullable)
    }
}

impl fmt::Display for Nullability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Nullability::Nullable => write!(f, "NULL"),
            Nullability::NotNullable => write!(f, "NOT NULL"),
        }
    }
}

impl From<bool> for Nullability {
    fn from(nullable: bool) -> Self {
        if nullable {
            Nullability::Nullable
        } else {
            Nullability::NotNullable
        }
    }
}

impl From<Nullability> for bool {
    fn from(nullability: Nullability) -> Self {
        nullability.is_nullable()
    }
}

// =============================================================================
// ColumnDefinition
// =============================================================================

/// Definition of a table column.
///
/// This struct holds the complete definition of a column including its name,
/// SQL type, and nullability.
///
/// # Example
///
/// ```
/// use hyperdb_api_core::types::{ColumnDefinition, SqlType, Nullability};
///
/// let col = ColumnDefinition::new("id", SqlType::int(), Nullability::NotNullable);
/// assert_eq!(col.name, "id");
/// assert_eq!(col.sql_type, SqlType::Int);
/// assert!(!col.nullability.is_nullable());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    /// The column name.
    pub name: String,
    /// The SQL type of the column.
    pub sql_type: SqlType,
    /// Whether the column allows NULL values.
    pub nullability: Nullability,
}

impl ColumnDefinition {
    /// Creates a new column definition.
    ///
    /// # Arguments
    ///
    /// * `name` - The column name
    /// * `sql_type` - The SQL type
    /// * `nullability` - Whether the column allows NULL
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::{ColumnDefinition, SqlType, Nullability};
    ///
    /// let col = ColumnDefinition::new("price", SqlType::numeric(18, 2), Nullability::Nullable);
    /// ```
    pub fn new(name: impl Into<String>, sql_type: SqlType, nullability: Nullability) -> Self {
        ColumnDefinition {
            name: name.into(),
            sql_type,
            nullability,
        }
    }

    /// Creates a nullable column definition.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::{ColumnDefinition, SqlType};
    ///
    /// let col = ColumnDefinition::nullable("description", SqlType::text());
    /// assert!(col.nullability.is_nullable());
    /// ```
    pub fn nullable(name: impl Into<String>, sql_type: SqlType) -> Self {
        Self::new(name, sql_type, Nullability::Nullable)
    }

    /// Creates a non-nullable column definition.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::types::{ColumnDefinition, SqlType};
    ///
    /// let col = ColumnDefinition::not_null("id", SqlType::int());
    /// assert!(!col.nullability.is_nullable());
    /// ```
    pub fn not_null(name: impl Into<String>, sql_type: SqlType) -> Self {
        Self::new(name, sql_type, Nullability::NotNullable)
    }

    /// Returns the column's OID.
    pub fn oid(&self) -> u32 {
        self.sql_type.internal_oid()
    }
}

impl fmt::Display for ColumnDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\"{}\" {} {}",
            self.name, self.sql_type, self.nullability
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sql_type_creation() {
        assert_eq!(SqlType::int().internal_oid(), 23);
        assert_eq!(SqlType::text().internal_oid(), 25);
        assert_eq!(SqlType::big_int().internal_oid(), 20);
        assert_eq!(SqlType::double().internal_oid(), 701);
    }

    #[test]
    fn test_sql_type_display() {
        assert_eq!(SqlType::int().to_string(), "INTEGER");
        assert_eq!(SqlType::numeric(18, 2).to_string(), "NUMERIC(18, 2)");
        assert_eq!(SqlType::varchar(Some(255)).to_string(), "VARCHAR(255)");
        assert_eq!(SqlType::varchar(None).to_string(), "VARCHAR");
    }

    #[test]
    fn test_sql_type_from_oid() {
        assert_eq!(SqlType::from_oid(23), SqlType::Int);
        assert_eq!(SqlType::from_oid(25), SqlType::Text);
        assert_eq!(SqlType::from_oid(701), SqlType::Double);
    }

    #[test]
    fn test_sql_type_from_oid_and_modifier() {
        // Numeric with precision 18, scale 2
        // Modifier = ((18 << 16) | 2) + 4 = 1179654
        let numeric = SqlType::from_oid_and_modifier(1700, 1179654);
        assert_eq!(
            numeric,
            SqlType::Numeric {
                precision: 18,
                scale: 2
            }
        );

        // Varchar(255)
        // Modifier = 255 + 4 = 259
        let varchar = SqlType::from_oid_and_modifier(1043, 259);
        assert_eq!(
            varchar,
            SqlType::Varchar {
                max_length: Some(255)
            }
        );
    }

    #[test]
    fn test_numeric_modifier_bounds_checking() {
        // Valid cases
        // NUMERIC(38, 0) - max precision
        let modifier = (38 << 16) + 4;
        let numeric = SqlType::from_oid_and_modifier(1700, modifier);
        assert_eq!(
            numeric,
            SqlType::Numeric {
                precision: 38,
                scale: 0
            }
        );

        // NUMERIC(10, 2) - common case
        let modifier = ((10 << 16) | 2) + 4;
        let numeric = SqlType::from_oid_and_modifier(1700, modifier);
        assert_eq!(
            numeric,
            SqlType::Numeric {
                precision: 10,
                scale: 2
            }
        );

        // NUMERIC(1, 0) - minimum valid precision
        let modifier = (1 << 16) + 4;
        let numeric = SqlType::from_oid_and_modifier(1700, modifier);
        assert_eq!(
            numeric,
            SqlType::Numeric {
                precision: 1,
                scale: 0
            }
        );

        // Invalid cases - all should return precision=0, scale=0

        // modifier <= 0: unspecified
        assert_eq!(
            SqlType::from_oid_and_modifier(1700, 0),
            SqlType::Numeric {
                precision: 0,
                scale: 0
            }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1700, -1),
            SqlType::Numeric {
                precision: 0,
                scale: 0
            }
        );

        // modifier in [1, 3]: malformed (would cause integer underflow if unchecked)
        assert_eq!(
            SqlType::from_oid_and_modifier(1700, 1),
            SqlType::Numeric {
                precision: 0,
                scale: 0
            }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1700, 2),
            SqlType::Numeric {
                precision: 0,
                scale: 0
            }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1700, 3),
            SqlType::Numeric {
                precision: 0,
                scale: 0
            }
        );

        // precision > 38: exceeds MAX_PRECISION
        let modifier = (39 << 16) + 4;
        assert_eq!(
            SqlType::from_oid_and_modifier(1700, modifier),
            SqlType::Numeric {
                precision: 0,
                scale: 0
            }
        );

        // scale > precision: invalid (scale cannot exceed precision)
        let modifier = (10 << 16 | 0x000b) + 4;
        assert_eq!(
            SqlType::from_oid_and_modifier(1700, modifier),
            SqlType::Numeric {
                precision: 0,
                scale: 0
            }
        );
    }

    #[test]
    fn test_varchar_modifier_bounds_checking() {
        // Valid cases
        assert_eq!(
            SqlType::from_oid_and_modifier(1043, 259), // 255 + 4
            SqlType::Varchar {
                max_length: Some(255)
            }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1043, 4), // 0 + 4 (VARCHAR(0))
            SqlType::Varchar {
                max_length: Some(0)
            }
        );

        // Invalid cases - all should return max_length=None
        assert_eq!(
            SqlType::from_oid_and_modifier(1043, 0),
            SqlType::Varchar { max_length: None }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1043, -1),
            SqlType::Varchar { max_length: None }
        );
        // Malformed modifiers [1, 3]
        assert_eq!(
            SqlType::from_oid_and_modifier(1043, 1),
            SqlType::Varchar { max_length: None }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1043, 2),
            SqlType::Varchar { max_length: None }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1043, 3),
            SqlType::Varchar { max_length: None }
        );
    }

    #[test]
    fn test_char_modifier_bounds_checking() {
        // Valid cases
        assert_eq!(
            SqlType::from_oid_and_modifier(1042, 14), // 10 + 4 (CHAR(10))
            SqlType::Char { length: 10 }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1042, 5), // 1 + 4 (CHAR(1))
            SqlType::Char { length: 1 }
        );

        // Invalid cases - all should return length=1 (default)
        assert_eq!(
            SqlType::from_oid_and_modifier(1042, 0),
            SqlType::Char { length: 1 }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1042, -1),
            SqlType::Char { length: 1 }
        );
        // Malformed modifiers [1, 3]
        assert_eq!(
            SqlType::from_oid_and_modifier(1042, 1),
            SqlType::Char { length: 1 }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1042, 2),
            SqlType::Char { length: 1 }
        );
        assert_eq!(
            SqlType::from_oid_and_modifier(1042, 3),
            SqlType::Char { length: 1 }
        );
    }

    #[test]
    fn test_sql_type_categories() {
        assert!(SqlType::int().is_numeric());
        assert!(SqlType::double().is_numeric());
        assert!(!SqlType::text().is_numeric());

        assert!(SqlType::text().is_string());
        assert!(SqlType::varchar(None).is_string());
        assert!(!SqlType::int().is_string());

        assert!(SqlType::date().is_temporal());
        assert!(SqlType::timestamp().is_temporal());
        assert!(!SqlType::int().is_temporal());
    }

    #[test]
    fn test_nullability() {
        assert!(Nullability::Nullable.is_nullable());
        assert!(!Nullability::NotNullable.is_nullable());

        let nullable: Nullability = true.into();
        assert!(nullable.is_nullable());

        let not_null: Nullability = false.into();
        assert!(!not_null.is_nullable());
    }

    #[test]
    fn test_column_definition() {
        let col = ColumnDefinition::new("id", SqlType::int(), Nullability::NotNullable);
        assert_eq!(col.name, "id");
        assert_eq!(col.sql_type, SqlType::Int);
        assert!(!col.nullability.is_nullable());

        let nullable_col = ColumnDefinition::nullable("name", SqlType::text());
        assert!(nullable_col.nullability.is_nullable());

        let not_null_col = ColumnDefinition::not_null("count", SqlType::big_int());
        assert!(!not_null_col.nullability.is_nullable());
    }

    #[test]
    fn test_column_definition_display() {
        let col = ColumnDefinition::new("id", SqlType::int(), Nullability::NotNullable);
        assert_eq!(col.to_string(), "\"id\" INTEGER NOT NULL");

        let col2 = ColumnDefinition::nullable("name", SqlType::text());
        assert_eq!(col2.to_string(), "\"name\" TEXT NULL");
    }
}
