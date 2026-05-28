// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! [`RowAccessor`] — name-based column access for [`FromRow`] impls
//! with cached column-name → index resolution.
//!
//! When a typed query is consumed via [`Connection::fetch_one_as`] /
//! [`Connection::fetch_all_as`] (and the async equivalents), the engine
//! resolves each column name to its position in the result schema
//! **once per query** and hands a `RowAccessor` to every `FromRow` impl.
//! Inside a `FromRow` impl, calling `accessor.get("col")` is a single
//! `HashMap` lookup followed by typed access — no per-call linear scan
//! over the schema.
//!
//! For one-off named access on a [`Row`] (outside `fetch_*_as`), use
//! [`Row::get_by_name`] instead — it does a linear scan but doesn't
//! require building a cache.
//!
//! [`Connection::fetch_one_as`]: crate::Connection::fetch_one_as
//! [`Connection::fetch_all_as`]: crate::Connection::fetch_all_as
//! [`Row::get_by_name`]: crate::Row::get_by_name

use std::collections::HashMap;

use crate::error::{ColumnErrorKind, Error, Result};
use crate::result::{Row, RowValue};

/// A view over a [`Row`] that supports name-based access via a
/// pre-resolved column-name → index lookup table.
///
/// `RowAccessor` is the parameter type of [`FromRow::from_row`]; it
/// borrows the row and a shared lookup map built once per query in
/// [`fetch_one_as`](crate::Connection::fetch_one_as) /
/// [`fetch_all_as`](crate::Connection::fetch_all_as).
///
/// # Example
///
/// ```no_run
/// use hyperdb_api::{FromRow, RowAccessor, Result};
///
/// struct User { id: i32, name: String, email: Option<String> }
///
/// impl FromRow for User {
///     fn from_row(row: RowAccessor<'_>) -> Result<Self> {
///         Ok(User {
///             id: row.get("id")?,
///             name: row.get("name")?,
///             email: row.get_opt("email")?,
///         })
///     }
/// }
/// ```
#[derive(Debug)]
pub struct RowAccessor<'a> {
    row: &'a Row,
    indices: &'a HashMap<&'a str, usize>,
}

impl<'a> RowAccessor<'a> {
    /// Constructs a new `RowAccessor` over the given row and pre-built
    /// lookup map. Crate-internal: callers go through `fetch_*_as` to
    /// get a `RowAccessor`, never construct one directly.
    pub(crate) fn new(row: &'a Row, indices: &'a HashMap<&'a str, usize>) -> Self {
        Self { row, indices }
    }

    /// Builds a `name → index` lookup table from a [`ResultSchema`].
    ///
    /// Used by `fetch_*_as` to resolve names once per query before
    /// iterating rows. Consumes O(N) time and allocates one entry per
    /// column.
    ///
    /// [`ResultSchema`]: crate::ResultSchema
    pub(crate) fn build_indices(schema: &'a crate::ResultSchema) -> HashMap<&'a str, usize> {
        let mut map = HashMap::with_capacity(schema.column_count());
        for i in 0..schema.column_count() {
            map.insert(schema.column(i).name(), i);
        }
        map
    }

    /// Returns the underlying [`Row`].
    ///
    /// Useful for callers that need access to row methods not exposed
    /// through this accessor (e.g. positional `Row::get`).
    #[must_use]
    pub fn row(&self) -> &Row {
        self.row
    }

    /// Returns the named column's value, decoded as `T`.
    ///
    /// # Errors
    ///
    /// - [`Error::Column`] with [`ColumnErrorKind::Missing`] if `name`
    ///   is not in the result schema.
    /// - [`Error::Column`] with [`ColumnErrorKind::Null`] if the cell
    ///   is SQL `NULL`.
    /// - [`Error::Column`] with [`ColumnErrorKind::TypeMismatch`] if
    ///   the cell value cannot be decoded as `T`.
    pub fn get<T: RowValue>(&self, name: &str) -> Result<T> {
        let idx = self
            .indices
            .get(name)
            .copied()
            .ok_or_else(|| Error::column(name, ColumnErrorKind::Missing))?;
        match self.row.get::<T>(idx) {
            Some(v) => Ok(v),
            None => {
                // Disambiguate NULL from type-mismatch by re-checking
                // the underlying cell. `row.is_null(idx)` is the source
                // of truth for SQL NULL.
                if self.row.is_null(idx) {
                    Err(Error::column(name, ColumnErrorKind::Null))
                } else {
                    let actual = self
                        .row
                        .sql_type(idx)
                        .map_or_else(|| "<unknown>".to_string(), |t| format!("{t:?}"));
                    Err(Error::column(
                        name,
                        ColumnErrorKind::TypeMismatch {
                            expected: std::any::type_name::<T>().to_string(),
                            actual,
                        },
                    ))
                }
            }
        }
    }

    /// Returns the named column's value as `Option<T>`. SQL `NULL`
    /// becomes `None`; missing columns and type mismatches still error.
    ///
    /// # Errors
    ///
    /// - [`Error::Column`] with [`ColumnErrorKind::Missing`] if `name`
    ///   is not in the result schema.
    /// - [`Error::Column`] with [`ColumnErrorKind::TypeMismatch`] if
    ///   the cell is non-NULL but cannot be decoded as `T`.
    pub fn get_opt<T: RowValue>(&self, name: &str) -> Result<Option<T>> {
        let idx = self
            .indices
            .get(name)
            .copied()
            .ok_or_else(|| Error::column(name, ColumnErrorKind::Missing))?;
        if self.row.is_null(idx) {
            return Ok(None);
        }
        if let Some(v) = self.row.get::<T>(idx) {
            Ok(Some(v))
        } else {
            let actual = self
                .row
                .sql_type(idx)
                .map_or_else(|| "<unknown>".to_string(), |t| format!("{t:?}"));
            Err(Error::column(
                name,
                ColumnErrorKind::TypeMismatch {
                    expected: std::any::type_name::<T>().to_string(),
                    actual,
                },
            ))
        }
    }

    /// Positional escape hatch: returns the value at column `idx`,
    /// decoded as `T`.
    ///
    /// # Errors
    ///
    /// - [`Error::ColumnIndexOutOfBounds`] if `idx` is past the row's
    ///   column count.
    /// - [`Error::Conversion`] if the cell is `NULL` or cannot be
    ///   decoded as `T`. (Wraps [`Row::try_get`].)
    pub fn position<T: RowValue>(&self, idx: usize) -> Result<T> {
        if idx >= self.row.column_count() {
            return Err(Error::column_index_out_of_bounds(
                idx,
                self.row.column_count(),
            ));
        }
        // Reuse Row::try_get's NULL/decode-error path. Synthesize a
        // column-name label for the error message.
        let label = format!("col[{idx}]");
        self.row.try_get::<T>(idx, &label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::{ResultColumn, ResultSchema};
    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType as ArrowType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use hyperdb_api_core::types::SqlType;
    use std::sync::Arc;

    /// Build a single-row `(id INT, name TEXT)` Arrow batch + matching
    /// `ResultSchema` for use in `RowAccessor` unit tests.
    fn user_row(id: Option<i32>, name: Option<&str>) -> (Row, Arc<ResultSchema>) {
        let id_array = Int32Array::from(vec![id]);
        let name_array = StringArray::from(vec![name]);
        let arrow_schema = Arc::new(Schema::new(vec![
            Field::new("id", ArrowType::Int32, true),
            Field::new("name", ArrowType::Utf8, true),
        ]));
        let batch = Arc::new(
            RecordBatch::try_new(arrow_schema, vec![Arc::new(id_array), Arc::new(name_array)])
                .expect("batch"),
        );
        let schema = Arc::new(ResultSchema::from_columns(vec![
            ResultColumn::new("id", SqlType::int(), 0),
            ResultColumn::new("name", SqlType::text(), 1),
        ]));
        let row = Row::from_arrow(batch, 0, Some(Arc::clone(&schema)));
        (row, schema)
    }

    #[test]
    fn missing_column_errors_with_kind_missing() {
        let (row, schema) = user_row(Some(1), Some("alice"));
        let indices = RowAccessor::build_indices(&schema);
        let accessor = RowAccessor::new(&row, &indices);

        let err = accessor.get::<i32>("does_not_exist").unwrap_err();
        match err {
            Error::Column { name, kind } => {
                assert_eq!(name, "does_not_exist");
                assert!(matches!(kind, ColumnErrorKind::Missing));
            }
            other => panic!("expected Error::Column {{ kind: Missing }}, got {other:?}"),
        }
    }

    #[test]
    fn null_in_required_column_errors_with_kind_null() {
        let (row, schema) = user_row(Some(1), None);
        let indices = RowAccessor::build_indices(&schema);
        let accessor = RowAccessor::new(&row, &indices);

        let err = accessor.get::<String>("name").unwrap_err();
        match err {
            Error::Column { name, kind } => {
                assert_eq!(name, "name");
                assert!(matches!(kind, ColumnErrorKind::Null));
            }
            other => panic!("expected Error::Column {{ kind: Null }}, got {other:?}"),
        }
    }

    #[test]
    fn null_in_optional_column_returns_none() {
        let (row, schema) = user_row(Some(1), None);
        let indices = RowAccessor::build_indices(&schema);
        let accessor = RowAccessor::new(&row, &indices);

        let v: Option<String> = accessor.get_opt("name").expect("get_opt for NULL");
        assert_eq!(v, None);
    }

    #[test]
    fn happy_path_get_returns_value() {
        let (row, schema) = user_row(Some(42), Some("alice"));
        let indices = RowAccessor::build_indices(&schema);
        let accessor = RowAccessor::new(&row, &indices);

        let id: i32 = accessor.get("id").expect("get id");
        let name: String = accessor.get("name").expect("get name");
        assert_eq!(id, 42);
        assert_eq!(name, "alice");
    }

    #[test]
    fn happy_path_get_opt_returns_some() {
        let (row, schema) = user_row(Some(42), Some("alice"));
        let indices = RowAccessor::build_indices(&schema);
        let accessor = RowAccessor::new(&row, &indices);

        let id: Option<i32> = accessor.get_opt("id").expect("get_opt id");
        let name: Option<String> = accessor.get_opt("name").expect("get_opt name");
        assert_eq!(id, Some(42));
        assert_eq!(name, Some("alice".to_string()));
    }

    #[test]
    fn position_out_of_range_errors_with_index_oob() {
        let (row, schema) = user_row(Some(1), Some("alice"));
        let indices = RowAccessor::build_indices(&schema);
        let accessor = RowAccessor::new(&row, &indices);

        // Row has 2 columns; position 5 is out of range.
        let err = accessor.position::<i32>(5).unwrap_err();
        match err {
            Error::ColumnIndexOutOfBounds { idx, column_count } => {
                assert_eq!(idx, 5);
                assert_eq!(column_count, 2);
            }
            other => panic!("expected Error::ColumnIndexOutOfBounds, got {other:?}"),
        }
    }

    #[test]
    fn position_in_range_returns_value() {
        let (row, schema) = user_row(Some(42), Some("alice"));
        let indices = RowAccessor::build_indices(&schema);
        let accessor = RowAccessor::new(&row, &indices);

        let id: i32 = accessor.position(0).expect("position 0");
        assert_eq!(id, 42);
    }
}
