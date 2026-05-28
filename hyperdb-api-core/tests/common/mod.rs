// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Common test utilities for hyper-client integration tests.
//!
//! These utilities provide a test environment with a running Hyper server.
//! We use `hyperdb_api::HyperProcess` as a dev-dependency to manage the server lifecycle.

use hyperdb_api::{HyperProcess, Parameters};
use hyperdb_api_core::client::{Client, Config};
use std::path::PathBuf;

/// Returns the path to a test result file in the `test_results` directory.
///
/// Creates the `test_results` directory if it doesn't exist.
/// Files are named after the test/example that created them.
/// Returns an absolute path for use with CREATE DATABASE.
pub(crate) fn test_result_path(name: &str, extension: &str) -> hyperdb_api::Result<PathBuf> {
    let test_results_dir = std::env::current_dir()?.join("test_results");
    std::fs::create_dir_all(&test_results_dir)?;
    // Canonicalize the directory (which exists), then join the filename
    let canonical_dir = test_results_dir.canonicalize().unwrap_or(test_results_dir);
    Ok(canonical_dir.join(format!("{name}.{extension}")))
}

/// Creates `HyperProcess` parameters configured to write logs to `test_results` directory.
pub(crate) fn test_hyper_params(_test_name: &str) -> hyperdb_api::Result<Parameters> {
    let test_results_dir = std::env::current_dir()?.join("test_results");
    std::fs::create_dir_all(&test_results_dir)?;
    let mut params = Parameters::new();
    // Use absolute path for log_dir
    let log_dir = test_results_dir.canonicalize().unwrap_or(test_results_dir);
    params.set("log_dir", log_dir.to_string_lossy().to_string());
    Ok(params)
}

/// Test helper that manages a Hyper process and provides both low-level
/// client access and high-level connection access.
///
/// This allows testing the `hyper-client` API directly against a real server.
#[expect(
    dead_code,
    reason = "test helper; referenced by subset of test binaries in this crate"
)]
pub(crate) struct TestServer {
    /// The Hyper process instance.
    pub hyper: HyperProcess,
    /// The server's host address.
    pub host: String,
    /// The server's port.
    pub port: u16,
    /// Path to the database file.
    pub database_path: PathBuf,
}

impl TestServer {
    /// Creates a new test server with a temporary database.
    ///
    /// The Hyper process and database are automatically cleaned up when dropped.
    /// Uses the current test function name if available, otherwise "test".
    #[allow(
        dead_code,
        reason = "test helper; referenced by subset of test binaries in this crate"
    )]
    pub(crate) fn new() -> hyperdb_api::Result<Self> {
        let thread = std::thread::current();
        let test_name = thread
            .name()
            .and_then(|n| n.split("::").last())
            .map_or_else(|| "test".to_string(), std::string::ToString::to_string);
        Self::with_name(&test_name)
    }

    /// Creates a new test server with a database named after the test.
    #[allow(
        dead_code,
        reason = "test helper; referenced by subset of test binaries in this crate"
    )]
    pub(crate) fn with_name(test_name: &str) -> hyperdb_api::Result<Self> {
        let database_path = test_result_path(test_name, "hyper")?;
        let params = test_hyper_params(test_name)?;
        let hyper = HyperProcess::new(None, Some(&params))?;

        // Parse endpoint to get host and port
        let endpoint = hyper.endpoint().expect("No endpoint available");
        let endpoint_str = endpoint.to_string();
        let (host, port) = parse_endpoint(&endpoint_str);

        // Create a config to connect without a database first
        let config = Config::new()
            .with_host(&host)
            .with_port(port)
            .with_user("tableau_internal_user");

        // Connect without a database, then create the database via SQL
        let client = Client::connect(&config)
            .map_err(|e| hyperdb_api::Error::internal(format!("Failed to connect: {e}")))?;

        // Drop database if it exists (from previous test run), then create it
        let db_path_escaped = database_path.to_string_lossy().replace('"', "\"\"");
        let _ = client.exec(&format!("DROP DATABASE IF EXISTS \"{db_path_escaped}\""));
        client
            .exec(&format!("CREATE DATABASE \"{db_path_escaped}\""))
            .map_err(|e| hyperdb_api::Error::internal(format!("Failed to create database: {e}")))?;

        client
            .close()
            .map_err(|e| hyperdb_api::Error::internal(format!("Failed to close: {e}")))?;

        Ok(TestServer {
            hyper,
            host,
            port,
            database_path,
        })
    }

    /// Creates a new test server without pre-creating a database.
    /// Uses the current test function name if available, otherwise "test".
    #[allow(
        dead_code,
        reason = "test helper; referenced by subset of test binaries in this crate"
    )]
    pub(crate) fn without_database() -> hyperdb_api::Result<Self> {
        let thread = std::thread::current();
        let test_name = thread
            .name()
            .and_then(|n| n.split("::").last())
            .map_or_else(|| "test".to_string(), std::string::ToString::to_string);
        Self::without_database_with_name(&test_name)
    }

    /// Creates a new test server without pre-creating a database, named after the test.
    #[allow(
        dead_code,
        reason = "test helper; referenced by subset of test binaries in this crate"
    )]
    pub(crate) fn without_database_with_name(test_name: &str) -> hyperdb_api::Result<Self> {
        let database_path = test_result_path(test_name, "hyper")?;
        let params = test_hyper_params(test_name)?;
        let hyper = HyperProcess::new(None, Some(&params))?;

        let endpoint = hyper.endpoint().expect("No endpoint available");
        let endpoint_str = endpoint.to_string();
        let (host, port) = parse_endpoint(&endpoint_str);

        Ok(TestServer {
            hyper,
            host,
            port,
            database_path,
        })
    }

    /// Creates a `Config` for connecting to this test server.
    #[allow(
        dead_code,
        reason = "test helper; referenced by subset of test binaries in this crate"
    )]
    pub(crate) fn config(&self) -> Config {
        Config::new()
            .with_host(&self.host)
            .with_port(self.port)
            .with_user("tableau_internal_user")
            .with_database(self.database_path.to_string_lossy().to_string())
    }

    /// Creates a `Config` without a database (for connecting without attaching).
    #[allow(
        dead_code,
        reason = "test helper; referenced by subset of test binaries in this crate"
    )]
    pub(crate) fn config_without_database(&self) -> Config {
        Config::new()
            .with_host(&self.host)
            .with_port(self.port)
            .with_user("tableau_internal_user")
    }

    /// Connects a `Client` to this test server's database.
    #[allow(
        dead_code,
        reason = "test helper; referenced by subset of test binaries in this crate"
    )]
    pub(crate) fn connect(&self) -> hyperdb_api_core::client::Result<Client> {
        let config = self.config();
        Client::connect(&config)
    }

    /// Connects a `Client` to this test server without a database.
    #[allow(
        dead_code,
        reason = "test helper; referenced by subset of test binaries in this crate"
    )]
    pub(crate) fn connect_without_database(&self) -> hyperdb_api_core::client::Result<Client> {
        let config = self.config_without_database();
        Client::connect(&config)
    }

    /// Returns the database path as a string.
    #[expect(
        dead_code,
        reason = "test helper; referenced by subset of test binaries in this crate"
    )]
    pub(crate) fn database_path_str(&self) -> String {
        self.database_path.to_string_lossy().to_string()
    }
}

/// Parses a Hyper endpoint string like "<tab.tcp://localhost:12345>" into (host, port).
fn parse_endpoint(endpoint: &str) -> (String, u16) {
    // Remove protocol prefix if present
    let addr = endpoint.strip_prefix("tab.tcp://").unwrap_or(endpoint);

    // Split host and port
    if let Some((host, port_str)) = addr.rsplit_once(':') {
        let port = port_str.parse().unwrap_or(7483);
        (host.to_string(), port)
    } else {
        (addr.to_string(), 7483)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_endpoint() {
        assert_eq!(
            parse_endpoint("tab.tcp://localhost:12345"),
            ("localhost".to_string(), 12345)
        );
        assert_eq!(
            parse_endpoint("127.0.0.1:7483"),
            ("127.0.0.1".to_string(), 7483)
        );
    }
}
