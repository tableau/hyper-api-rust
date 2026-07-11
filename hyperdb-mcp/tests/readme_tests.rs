// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Structural checks on the `get_readme` payload defined in
//! [`hyperdb_mcp::readme`].
//!
//! These tests don't lock in exact prose — they pin the README to the
//! actual tool surface and to the few invariants we always want
//! present. If a tool is added or renamed, `readme_mentions_every_tool`
//! fails until [`hyperdb-mcp/src/readme.rs`] is updated to match.

use hyperdb_mcp::readme::README;

#[test]
fn readme_is_non_trivial() {
    assert!(
        README.len() > 500,
        "README looks empty/stub: {} bytes",
        README.len()
    );
    assert!(
        README.len() < 20_000,
        "README is too long for a tool response: {} bytes",
        README.len()
    );
}

#[test]
fn readme_mentions_every_tool() {
    // If you add or rename a tool, update this list AND the README.
    // The two must stay in sync.
    let tools = [
        "query",
        "query_data",
        "query_file",
        "execute",
        "load_file",
        "load_files",
        "load_data",
        "load_iceberg",
        "describe",
        "sample",
        "inspect_file",
        "status",
        "export",
        "chart",
        "copy_query",
        "save_query",
        "delete_query",
        "set_table_metadata",
        "attach_database",
        "detach_database",
        "list_attached_databases",
        "watch_directory",
        "unwatch_directory",
        "kv_get",
        "kv_set",
        "kv_set_many",
        "kv_delete",
        "kv_list",
        "kv_list_stores",
        "kv_size",
        "kv_pop",
        "kv_clear",
        "get_readme",
    ];
    for tool in tools {
        assert!(README.contains(tool), "README missing mention of `{tool}`");
    }
}

#[test]
fn readme_includes_sql_dialect_pointers() {
    assert!(
        README.contains("PostgreSQL"),
        "README should call out PostgreSQL compatibility"
    );
    assert!(
        README.contains("read-only"),
        "README should mention read-only mode constraints"
    );
    assert!(
        README.contains("Hyper"),
        "README should identify the underlying Hyper engine"
    );
}
