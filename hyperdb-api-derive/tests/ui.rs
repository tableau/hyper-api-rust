// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! W5 trybuild UI tests — compile-error golden files for every error path.
//!
//! Each `.rs` file in `tests/ui/` is compiled with trybuild. Files under
//! `tests/ui/pass/` must compile successfully; files under `tests/ui/fail/`
//! must fail to compile and their error output must match the corresponding
//! `.stderr` golden file.
//!
//! Run to update golden files:
//!   TRYBUILD=overwrite cargo test -p hyperdb-api-derive --test ui

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass/*.rs");
    t.compile_fail("tests/ui/fail/*.rs");
}
