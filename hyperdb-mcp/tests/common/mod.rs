// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shared test helpers for hyperdb-mcp integration tests.

use hyperdb_mcp::engine::Engine;
use tempfile::TempDir;

/// Wrapper that co-owns a temp directory alongside the Engine so that the
/// workspace `.hyper` file isn't deleted while tests are still running.
/// The `TempDir` is cleaned up automatically when the struct is dropped.
pub(crate) struct TestEngine {
    pub engine: Engine,
    /// Held to keep the temp directory alive for the duration of the test.
    pub _temp_dir: TempDir,
}

impl TestEngine {
    /// Spin up a fresh Engine backed by a temp directory. Each test gets its
    /// own `hyperd` process and empty workspace — no cross-test contamination.
    pub(crate) fn new_ephemeral() -> Self {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let workspace_path = temp_dir.path().join("workspace.hyper");
        let engine = Engine::new_no_daemon(Some(workspace_path.to_str().unwrap().to_string()))
            .expect("failed to create engine");
        Self {
            engine,
            _temp_dir: temp_dir,
        }
    }
}
