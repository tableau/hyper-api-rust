// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Runtime `Table` trait emitted by `#[derive(Table)]`.

/// Describes a database table derived from a Rust struct.
///
/// Implemented by `#[derive(Table)]`. Provides the SQL `CREATE TABLE`
/// statement as a `const`, which can be used at runtime for migrations,
/// test fixtures, or compile-time validation (via `#[hyperdb(register)]`).
///
/// # Example
///
/// ```rust,ignore
/// use hyperdb_api::{Table};
///
/// #[derive(Table)]
/// #[hyperdb(table = "users")]
/// struct User {
///     #[hyperdb(primary_key)]
///     id: i64,
///     name: String,
///     email: Option<String>,
/// }
///
/// println!("{}", User::CREATE_SQL);
/// // CREATE TABLE users (id BIGINT NOT NULL, name TEXT NOT NULL, email TEXT)
/// ```
pub trait Table {
    /// The SQL table name (lower-snake-case of the struct name by default,
    /// or the value of `#[hyperdb(table = "...")]`).
    const NAME: &'static str;

    /// The full `CREATE TABLE` SQL statement for this struct's schema.
    const CREATE_SQL: &'static str;
}
