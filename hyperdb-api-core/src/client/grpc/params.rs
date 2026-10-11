// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Query parameter support for gRPC queries.
//!
//! This module provides types and utilities for parameterized queries over gRPC.
//! Parameters can be passed as JSON or Arrow IPC format.
//!
//! # Example
//!
//! ```no_run
//! use hyperdb_api_core::client::grpc::{GrpcClient, GrpcConfig, QueryParameters, ParameterStyle};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! # let config = GrpcConfig::new("http://localhost:7484");
//! let mut client = GrpcClient::connect(config).await?;
//!
//! // Using JSON parameters with $1, $2 style (json! values for mixed types)
//! let params = QueryParameters::json_positional(&[
//!     &serde_json::json!(42),
//!     &serde_json::json!("hello"),
//! ])?;
//! let result = client.execute_query_with_params(
//!     "SELECT * FROM users WHERE id = $1 AND name = $2",
//!     params,
//!     ParameterStyle::DollarNumbered,
//! ).await?;
//!
//! // Using JSON parameters with named style. A keyword such as `name` must
//! // be quoted (`:"name"`) to be used as a parameter name.
//! let params = QueryParameters::json_named()
//!     .add("id", &42i64)?
//!     .add("user_name", &"hello")?
//!     .build();
//! let result = client.execute_query_with_params(
//!     "SELECT * FROM users WHERE id = :id AND name = :user_name",
//!     params,
//!     ParameterStyle::Named,
//! ).await?;
//! # Ok(())
//! # }
//! ```

use bytes::Bytes;
use serde::Serialize;
use serde::ser::Error as _;
use serde_json::{Map as JsonMap, Value as JsonValue};

use super::proto::hyper_service::query_param::{ParameterStyle as ProtoParameterStyle, Parameters};
use super::proto::hyper_service::{QueryParameterArrow, QueryParameterJson};

/// Encodes one JSON value as a typed parameter entry in hyperd's JSON wire
/// format: `{"type": "<hyper type>", "value": "<text>"}`.
///
/// hyperd does not accept bare JSON values as parameters. It requires an
/// array of such entries, each carrying a Hyper type name and the value as
/// text (or `null`), which it then casts to that type. Values map to types as
/// follows:
///
/// | JSON value | Hyper type |
/// |---|---|
/// | boolean | `bool` |
/// | integer that fits `i64` | `bigint` |
/// | integer above `i64::MAX` | `numeric(20, 0)` |
/// | other number | `float8` |
/// | string | `varchar` |
/// | `null` | nullable `varchar` |
///
/// Arrays and objects have no unambiguous Hyper type and are rejected.
fn typed_entry(value: JsonValue) -> Result<JsonMap<String, JsonValue>, serde_json::Error> {
    let mut entry = JsonMap::new();
    let (type_name, text) = match value {
        JsonValue::Null => return Ok(null_entry()),
        JsonValue::Bool(b) => ("bool", JsonValue::String(b.to_string())),
        JsonValue::Number(n) => {
            let type_name = if n.is_i64() {
                "bigint"
            } else if n.is_u64() {
                // u64::MAX has 20 decimal digits
                entry.insert("precision".into(), 20.into());
                entry.insert("scale".into(), 0.into());
                "numeric"
            } else {
                "float8"
            };
            (type_name, JsonValue::String(n.to_string()))
        }
        JsonValue::String(s) => ("varchar", JsonValue::String(s)),
        JsonValue::Array(_) | JsonValue::Object(_) => {
            return Err(serde_json::Error::custom(
                "array and object values cannot be sent as JSON query parameters; \
                 build the typed parameter JSON yourself and use from_json_string",
            ));
        }
    };
    entry.insert("type".into(), type_name.into());
    entry.insert("value".into(), text);
    Ok(entry)
}

/// The typed entry for a SQL NULL: a nullable `varchar` with a `null` value.
fn null_entry() -> JsonMap<String, JsonValue> {
    let mut entry = JsonMap::new();
    entry.insert("type".into(), "varchar".into());
    entry.insert("value".into(), JsonValue::Null);
    entry
}

/// Parameter style for SQL queries.
///
/// This determines how parameters are referenced in the SQL query string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParameterStyle {
    /// Use question marks: `SELECT * FROM users WHERE id = ?`
    #[default]
    QuestionMark,
    /// Use dollar-numbered placeholders: `SELECT * FROM users WHERE id = $1`
    DollarNumbered,
    /// Use named parameters with colon: `SELECT * FROM users WHERE id = :id`
    Named,
}

impl From<ParameterStyle> for i32 {
    fn from(style: ParameterStyle) -> Self {
        match style {
            ParameterStyle::QuestionMark => ProtoParameterStyle::QuestionMark as i32,
            ParameterStyle::DollarNumbered => ProtoParameterStyle::DollarNumbered as i32,
            ParameterStyle::Named => ProtoParameterStyle::Named as i32,
        }
    }
}

impl From<ParameterStyle> for ProtoParameterStyle {
    fn from(style: ParameterStyle) -> Self {
        match style {
            ParameterStyle::QuestionMark => ProtoParameterStyle::QuestionMark,
            ParameterStyle::DollarNumbered => ProtoParameterStyle::DollarNumbered,
            ParameterStyle::Named => ProtoParameterStyle::Named,
        }
    }
}

/// Query parameters for gRPC queries.
///
/// Parameters can be encoded as JSON or Arrow IPC format.
/// JSON is simpler but Arrow is more efficient for large parameter sets.
#[derive(Debug, Clone)]
pub enum QueryParameters {
    /// JSON-encoded parameters.
    Json(String),
    /// Arrow IPC-encoded parameters. Stored as `Bytes` so the parameter
    /// payload can be handed to prost without a copy.
    Arrow(Bytes),
}

impl QueryParameters {
    /// Creates JSON parameters from a JSON string, sent to hyperd as-is.
    ///
    /// hyperd expects an array with one typed entry per parameter:
    /// `{"type": <hyper type>, "value": <text or null>}`, plus `"name"` for
    /// named parameters. `value` is always a JSON string (or `null`) that
    /// hyperd casts to `type`. Use this constructor when you need a type that
    /// [`json_positional`](Self::json_positional) does not infer, such as
    /// `date` or `numeric` with `precision`/`scale`.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::QueryParameters;
    ///
    /// // For positional parameters ($1, $2 or ?)
    /// let params = QueryParameters::from_json_string(
    ///     r#"[{"type": "integer", "value": "42"}, {"type": "date", "value": "2026-01-31"}]"#,
    /// );
    ///
    /// // For named parameters (:id, :name)
    /// let params = QueryParameters::from_json_string(
    ///     r#"[{"name": "id", "type": "bigint", "value": "42"},
    ///         {"name": "name", "type": "varchar", "value": null}]"#,
    /// );
    /// ```
    pub fn from_json_string(json: impl Into<String>) -> Self {
        QueryParameters::Json(json.into())
    }

    /// Creates JSON parameters from a serializable value, sent to hyperd
    /// as-is.
    ///
    /// The value must serialize to the typed-entry array described on
    /// [`from_json_string`](Self::from_json_string); it is not converted.
    /// To pass plain values, use [`json_positional`](Self::json_positional)
    /// or [`json_named`](Self::json_named) instead.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::QueryParameters;
    /// use serde_json::json;
    ///
    /// let params = QueryParameters::from_json_value(&json!([
    ///     {"type": "smallint", "value": "7"},
    ///     {"type": "numeric", "precision": 10, "scale": 2, "value": "12.50"},
    /// ]))?;
    /// # Ok::<(), serde_json::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a [`serde_json::Error`] if `value` cannot be serialized
    /// to JSON (for example, a type with a failing `Serialize` impl).
    pub fn from_json_value<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        let json = serde_json::to_string(value)?;
        Ok(QueryParameters::Json(json))
    }

    /// Creates JSON parameters for positional placeholders ($1, $2 or ?).
    ///
    /// Each value is serialized to JSON and encoded as a typed entry in the
    /// array format hyperd expects (`[{"type":"bigint","value":"42"}, ...]`).
    /// The Hyper type is inferred from the JSON value: booleans become
    /// `bool`, integers `bigint` (or `numeric` above `i64::MAX`), other
    /// numbers `float8`, strings `varchar`, and `null` a nullable `varchar`.
    /// For a different type, cast in SQL (`CAST(? AS DATE)`).
    ///
    /// The slice is homogeneous in `T`; pass `serde_json::Value`s to mix
    /// types.
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::QueryParameters;
    /// use serde_json::json;
    ///
    /// // Same-type parameters
    /// let params = QueryParameters::json_positional(&[&42i64, &100i64])?;
    ///
    /// // Mixed types
    /// let params = QueryParameters::json_positional(&[&json!(42), &json!("hello"), &json!(true)])?;
    /// # Ok::<(), serde_json::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a [`serde_json::Error`] if any element of `values` fails
    /// to serialize to JSON, or serializes to a JSON array or object.
    pub fn json_positional<T: Serialize + ?Sized>(
        values: &[&T],
    ) -> Result<Self, serde_json::Error> {
        let entries: Vec<JsonValue> = values
            .iter()
            .map(|value| typed_entry(serde_json::to_value(value)?).map(JsonValue::Object))
            .collect::<Result<_, _>>()?;
        let json = serde_json::to_string(&entries)?;
        Ok(QueryParameters::Json(json))
    }

    /// Creates a builder for named JSON parameters (:name style).
    ///
    /// # Example
    ///
    /// ```
    /// use hyperdb_api_core::client::grpc::QueryParameters;
    ///
    /// let params = QueryParameters::json_named()
    ///     .add("id", &42i64)?
    ///     .add("name", &"Alice")?
    ///     .add("active", &true)?
    ///     .build();
    /// # Ok::<(), serde_json::Error>(())
    /// ```
    #[must_use]
    pub fn json_named() -> JsonNamedParamsBuilder {
        JsonNamedParamsBuilder::new()
    }

    /// Creates Arrow IPC parameters from raw bytes.
    ///
    /// The bytes should contain an Arrow IPC stream with schema and a single
    /// record batch containing the parameter values.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use hyperdb_api_core::client::grpc::QueryParameters;
    /// use arrow::array::{Int64Array, StringArray};
    /// use arrow::datatypes::{DataType, Field, Schema};
    /// use arrow::record_batch::RecordBatch;
    /// use arrow::ipc::writer::StreamWriter;
    /// use std::sync::Arc;
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Create Arrow arrays for parameters
    /// let id_array = Int64Array::from(vec![42]);
    /// let name_array = StringArray::from(vec!["Alice"]);
    ///
    /// // Create schema and record batch
    /// let schema = Arc::new(Schema::new(vec![
    ///     Field::new("id", DataType::Int64, false),
    ///     Field::new("name", DataType::Utf8, false),
    /// ]));
    /// let batch = RecordBatch::try_new(schema.clone(), vec![
    ///     Arc::new(id_array),
    ///     Arc::new(name_array),
    /// ])?;
    ///
    /// // Serialize to IPC
    /// let mut buf = Vec::new();
    /// let mut writer = StreamWriter::try_new(&mut buf, &batch.schema())?;
    /// writer.write(&batch)?;
    /// writer.finish()?;
    ///
    /// let params = QueryParameters::from_arrow(buf);
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_arrow(data: impl Into<Bytes>) -> Self {
        QueryParameters::Arrow(data.into())
    }

    /// Converts to the proto Parameters type.
    pub(crate) fn into_proto(self) -> Parameters {
        match self {
            QueryParameters::Json(json) => {
                Parameters::JsonParameters(QueryParameterJson { data: json })
            }
            QueryParameters::Arrow(data) => {
                Parameters::ArrowParameters(QueryParameterArrow { data })
            }
        }
    }

    /// Returns true if this is JSON-encoded parameters.
    pub fn is_json(&self) -> bool {
        matches!(self, QueryParameters::Json(_))
    }

    /// Returns true if this is Arrow-encoded parameters.
    pub fn is_arrow(&self) -> bool {
        matches!(self, QueryParameters::Arrow(_))
    }
}

/// Builder for named JSON parameters.
///
/// # Example
///
/// ```
/// use hyperdb_api_core::client::grpc::QueryParameters;
///
/// let params = QueryParameters::json_named()
///     .add("user_id", &123)?
///     .add("email", &"user@example.com")?
///     .build();
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct JsonNamedParamsBuilder {
    /// Typed entries in insertion order, each carrying its `name`.
    params: Vec<JsonValue>,
}

impl JsonNamedParamsBuilder {
    /// Creates a new builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a named parameter.
    ///
    /// # Arguments
    ///
    /// * `name` - Parameter name (without the colon prefix)
    /// * `value` - Parameter value (must be JSON-serializable). Its Hyper
    ///   type is inferred as described on
    ///   [`QueryParameters::json_positional`].
    ///
    /// # Errors
    ///
    /// Returns a [`serde_json::Error`] if `value` cannot be serialized
    /// to JSON, or serializes to a JSON array or object.
    pub fn add<T: Serialize>(
        self,
        name: impl Into<String>,
        value: &T,
    ) -> Result<Self, serde_json::Error> {
        let entry = typed_entry(serde_json::to_value(value)?)?;
        Ok(self.push(name.into(), entry))
    }

    #[must_use]
    /// Adds a null parameter, sent as a nullable `varchar`.
    pub fn add_null(self, name: impl Into<String>) -> Self {
        self.push(name.into(), null_entry())
    }

    fn push(mut self, name: String, mut entry: JsonMap<String, JsonValue>) -> Self {
        entry.insert("name".into(), name.into());
        self.params.push(JsonValue::Object(entry));
        self
    }

    /// Builds the `QueryParameters`.
    ///
    /// # Panics
    ///
    /// Does not panic in practice. The `serde_json::to_string` call on a
    /// `Value` tree is infallible — serde_json only fails when a
    /// user-defined `Serialize` impl returns an error, which cannot happen
    /// for the already-converted `Value` payloads inserted via
    /// [`Self::add`] and [`Self::add_null`].
    #[must_use]
    pub fn build(self) -> QueryParameters {
        let json = serde_json::to_string(&self.params).expect("serializing a Value never fails");
        QueryParameters::Json(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    fn parsed(params: QueryParameters) -> JsonValue {
        match params {
            QueryParameters::Json(json) => serde_json::from_str(&json).unwrap(),
            QueryParameters::Arrow(_) => panic!("Expected JSON parameters"),
        }
    }

    #[test]
    fn test_json_positional_integers() {
        let params = QueryParameters::json_positional(&[&42i64, &-7i64]).unwrap();
        assert_eq!(
            parsed(params),
            json!([
                {"type": "bigint", "value": "42"},
                {"type": "bigint", "value": "-7"},
            ])
        );
    }

    #[test]
    fn test_json_positional_strings() {
        let params = QueryParameters::json_positional(&[&"hello", &"world"]).unwrap();
        assert_eq!(
            parsed(params),
            json!([
                {"type": "varchar", "value": "hello"},
                {"type": "varchar", "value": "world"},
            ])
        );
    }

    #[test]
    fn test_json_positional_mixed() {
        let params = QueryParameters::json_positional(&[
            &json!(true),
            &json!(1.5),
            &json!(u64::MAX),
            &json!(null),
        ])
        .unwrap();
        assert_eq!(
            parsed(params),
            json!([
                {"type": "bool", "value": "true"},
                {"type": "float8", "value": "1.5"},
                {"type": "numeric", "precision": 20, "scale": 0, "value": "18446744073709551615"},
                {"type": "varchar", "value": null},
            ])
        );
    }

    #[test]
    fn test_json_positional_rejects_arrays_and_objects() {
        assert!(QueryParameters::json_positional(&[&json!([1, 2])]).is_err());
        assert!(QueryParameters::json_positional(&[&json!({"a": 1})]).is_err());
        assert!(
            QueryParameters::json_named()
                .add("list", &vec![1, 2])
                .is_err()
        );
    }

    #[test]
    fn test_json_named() {
        let params = QueryParameters::json_named()
            .add("id", &42i64)
            .unwrap()
            .add("name", &"Alice")
            .unwrap()
            .add_null("optional")
            .build();
        assert_eq!(
            parsed(params),
            json!([
                {"name": "id", "type": "bigint", "value": "42"},
                {"name": "name", "type": "varchar", "value": "Alice"},
                {"name": "optional", "type": "varchar", "value": null},
            ])
        );
    }

    #[test]
    fn test_from_json_string() {
        let params = QueryParameters::from_json_string(r#"{"foo": "bar"}"#);
        assert!(params.is_json());
        assert!(!params.is_arrow());
    }

    #[test]
    fn test_arrow_params() {
        let params = QueryParameters::from_arrow(vec![1, 2, 3, 4]);
        assert!(params.is_arrow());
        assert!(!params.is_json());
    }

    #[test]
    fn test_parameter_style_conversion() {
        assert_eq!(i32::from(ParameterStyle::QuestionMark), 3);
        assert_eq!(i32::from(ParameterStyle::DollarNumbered), 1);
        assert_eq!(i32::from(ParameterStyle::Named), 2);
    }
}
