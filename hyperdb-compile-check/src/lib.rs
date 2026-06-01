// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compile-time SQL validation logic for `hyperdb-api`.
//!
//! This is a **regular library crate** (not a proc-macro crate) so that its
//! validation logic — registry, dry-run, SQLSTATE classification, name-subset
//! diff — can be unit-tested with standard `cargo test` without `trybuild`.
//!
//! The proc-macro shells in `hyperdb-api-derive` call into this crate when the
//! `compile-time` feature is enabled. No `syn`/`quote`/`proc-macro2` types
//! appear in this crate's public API.
//!
//! # Architecture
//!
//! ```text
//! hyperdb-api-derive  (proc-macro shell, thin)
//!   └─(compile-time feature)─→ hyperdb-compile-check  (this crate, testable)
//!                                  └─→ hyperdb-api  (HyperProcess, Connection, …)
//! ```
//!
//! The three-crate split avoids the circular dependency that would arise from
//! adding `hyperdb-api` as a direct dep of `hyperdb-api-derive` (which
//! `hyperdb-api` already depends on).

pub mod db;
pub mod diagnostic;
pub mod dry_run;
pub mod error_extract;
pub mod registry;
pub mod validate;

pub use db::CompileTimeDb;
pub use diagnostic::ValidationError;
pub use registry::Registry;
pub use validate::{validate_query_as, validate_scalar_sql};
