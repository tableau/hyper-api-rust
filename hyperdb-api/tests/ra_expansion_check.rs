// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A8 rust-analyzer expansion confirmation.
//!
//! PURPOSE: verify that editing the function body below does NOT cause
//! rust-analyzer to re-expand the `query_as!` macro. RA's salsa engine
//! memoizes macro expansion keyed on the input token tree; if expansion
//! only fires when the token tree changes, the compile-time validator is
//! IDE-safe.
//!
//! HOW TO USE THIS FILE:
//! 1. Open this file in VS Code with the rust-analyzer extension.
//! 2. Hover over `query_as!(User, "SELECT id, name FROM users")` — RA should
//!    show the expanded `QueryAs::new(...)` output. This confirms RA is
//!    expanding the macro.
//! 3. Edit the comment on the line marked "EDIT ME" below.
//! 4. Watch the RA status bar: if it does NOT re-expand the `query_as!` macro
//!    (i.e. the macro squiggle/hover doesn't flicker), the salsa memoization
//!    is working correctly. If it re-expands on every keystroke, flag it —
//!    that's the S2 kill criterion.
//!
//! Expected outcome: editing the comment does NOT trigger re-expansion.
//! Editing the SQL string literal DOES trigger re-expansion (correct behavior).
//!
//! This file is NOT a regular test — it has no assertions. Its only purpose
//! is to give RA a real proc-macro invocation to observe. Remove it once
//! the observation is confirmed.

use hyperdb_api_derive::{query_as, FromRow, Table};

#[derive(Debug, FromRow, Table)]
#[hyperdb(table = "users", register)]
#[allow(
    dead_code,
    reason = "A8 RA observation harness — fields not read in tests"
)]
struct User {
    id: i64,
    name: String,
}

#[allow(dead_code, reason = "A8 RA observation harness — not a real test")]
fn ra_check(conn: &hyperdb_api::Connection) -> hyperdb_api::Result<Vec<User>> {
    // EDIT ME: change this comment and watch whether RA re-expands the macro below.
    // Expected: no re-expansion triggered by editing this comment.
    let users = query_as!(User, "SELECT id, name FROM users").fetch_all(conn)?;
    Ok(users)
}
