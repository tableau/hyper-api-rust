// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Common test utilities and helpers.

// This module is compiled into every integration-test binary via `mod common;`,
// but each binary calls only a subset of these helpers, so any given helper is
// dead code in the binaries that don't reference it. `allow` (not `expect`)
// because the same item is live in other binaries — an `expect` would be
// unfulfilled there and trip `unfulfilled_lint_expectations` under `-D warnings`.
#![allow(
    dead_code,
    reason = "shared test helpers; each test binary uses only a subset"
)]

use hyperdb_api::{Catalog, Connection, CreateMode, HyperProcess, Parameters, Result};
use std::path::PathBuf;

/// Returns the path to a test result file in the `test_results` directory.
///
/// Creates the `test_results` directory if it doesn't exist.
/// Files are named after the test/example that created them.
/// Returns an absolute path for use with CREATE DATABASE.
pub(crate) fn test_result_path(name: &str, extension: &str) -> Result<PathBuf> {
    let test_results_dir = std::env::current_dir()?.join("test_results");
    std::fs::create_dir_all(&test_results_dir)?;
    // Canonicalize the directory (which exists), then join the filename
    let canonical_dir = test_results_dir.canonicalize().unwrap_or(test_results_dir);
    Ok(canonical_dir.join(format!("{name}.{extension}")))
}

/// Creates `HyperProcess` parameters configured to write logs to `test_results` directory.
pub(crate) fn test_hyper_params(_test_name: &str) -> Result<Parameters> {
    let test_results_dir = std::env::current_dir()?.join("test_results");
    std::fs::create_dir_all(&test_results_dir)?;
    let mut params = Parameters::new();
    // Use absolute path for log_dir
    let log_dir = test_results_dir.canonicalize().unwrap_or(test_results_dir);
    params.set("log_dir", log_dir.to_string_lossy().to_string());
    Ok(params)
}

/// Test helper that manages a Hyper process and provides a connection.
///
/// Similar to the C++ `SingleConnectionTest` class, this provides a convenient
/// way to set up a test environment with a Hyper instance and database connection.
pub(crate) struct TestConnection {
    pub hyper: HyperProcess,
    pub connection: Connection,
    pub database_path: PathBuf,
}

impl TestConnection {
    /// Creates a new test connection with a temporary database.
    ///
    /// The Hyper process and database are automatically cleaned up when dropped.
    /// Uses the current test function name if available, otherwise "test".
    /// Uses `CreateAndReplace` mode to ensure a clean database state for each test.
    pub(crate) fn new() -> Result<Self> {
        Self::with_create_mode(CreateMode::CreateAndReplace)
    }

    /// Creates a new test connection with the specified create mode.
    /// Uses the current test function name if available, otherwise "test".
    pub(crate) fn with_create_mode(create_mode: CreateMode) -> Result<Self> {
        let thread = std::thread::current();
        let test_name = thread
            .name()
            .and_then(|n| n.split("::").last())
            .map_or_else(|| "test".to_string(), std::string::ToString::to_string);
        Self::with_create_mode_and_name(create_mode, &test_name)
    }

    /// Creates a new test connection with the specified create mode and test name.
    pub(crate) fn with_create_mode_and_name(
        create_mode: CreateMode,
        test_name: &str,
    ) -> Result<Self> {
        let database_path = test_result_path(test_name, "hyper")?;
        let params = test_hyper_params(test_name)?;
        let hyper = HyperProcess::new(None, Some(&params))?;
        let connection = Connection::new(&hyper, &database_path, create_mode)?;

        Ok(TestConnection {
            hyper,
            connection,
            database_path,
        })
    }

    /// Executes a SQL command.
    pub(crate) fn execute_command(&self, sql: &str) -> Result<u64> {
        self.connection.execute_command(sql)
    }

    /// Executes a SQL query and returns the result.
    pub(crate) fn execute_query(&self, sql: &str) -> Result<hyperdb_api::Rowset<'_>> {
        self.connection.execute_query(sql)
    }

    /// Generic helper for executing scalar queries.
    ///
    /// This extracts the common pattern from scalar query methods to reduce
    /// code duplication and improve maintainability.
    ///
    /// Executes a scalar query and returns a single i32 value.
    pub(crate) fn execute_scalar_i32(&self, sql: &str) -> Result<i32> {
        self.connection
            .execute_scalar_query::<i32>(sql)?
            .ok_or_else(|| hyperdb_api::Error::conversion(format!("NULL value for query: {sql}")))
    }

    /// Executes a scalar query and returns a single i64 value.
    pub(crate) fn execute_scalar_i64(&self, sql: &str) -> Result<i64> {
        self.connection
            .execute_scalar_query::<i64>(sql)?
            .ok_or_else(|| hyperdb_api::Error::conversion(format!("NULL value for query: {sql}")))
    }

    /// Executes a scalar query and returns a single String value.
    pub(crate) fn execute_scalar_string(&self, sql: &str) -> Result<String> {
        self.connection
            .execute_scalar_query::<String>(sql)?
            .ok_or_else(|| hyperdb_api::Error::conversion(format!("NULL value for query: {sql}")))
    }

    /// Executes a scalar query and returns a single bool value.
    pub(crate) fn execute_scalar_bool(&self, sql: &str) -> Result<bool> {
        self.connection
            .execute_scalar_query::<bool>(sql)?
            .ok_or_else(|| hyperdb_api::Error::conversion(format!("NULL value for query: {sql}")))
    }

    /// Counts the number of tuples in a table.
    pub(crate) fn count_tuples(&self, table_name: &str) -> Result<i64> {
        self.execute_scalar_i64(&format!("SELECT COUNT(*) FROM {table_name}"))
    }

    /// Gets a reference to the connection's catalog.
    pub(crate) fn catalog(&self) -> Catalog<'_> {
        Catalog::new(&self.connection)
    }
}

impl Default for TestConnection {
    fn default() -> Self {
        Self::new().expect("Failed to create test connection")
    }
}
