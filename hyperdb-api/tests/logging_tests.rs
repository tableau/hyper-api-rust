// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tests for structured logging with the tracing crate.
//!
//! These tests verify that the correct log events are emitted at the right times.
//!
//! Note: These tests use a custom `MockSubscriber` to capture logs from all targets,
//! since `tracing_test`'s default behavior doesn't capture logs from dependencies.

use hyperdb_api::{CreateMode, HyperProcess, Inserter, Result, SqlType, TableDefinition};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use tracing::subscriber::set_default;

mod common;
use common::TestConnection;

/// A simple mock subscriber that captures all log events for testing.
struct MockSubscriber {
    logs: Arc<Mutex<Vec<String>>>,
}

impl MockSubscriber {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let logs = Arc::new(Mutex::new(Vec::new()));
        (
            MockSubscriber {
                logs: Arc::clone(&logs),
            },
            logs,
        )
    }
}

impl tracing::Subscriber for MockSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let metadata = event.metadata();
        let target = metadata.target();
        let level = metadata.level();

        // Capture the message by visiting the fields
        struct MessageVisitor(String);
        impl tracing::field::Visit for MessageVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if !self.0.is_empty() {
                    self.0.push(' ');
                }
                let _ = write!(self.0, "{}={:?}", field.name(), value);
            }

            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if !self.0.is_empty() {
                    self.0.push(' ');
                }
                if field.name() == "message" {
                    self.0.push_str(value);
                } else {
                    let _ = write!(self.0, "{}={}", field.name(), value);
                }
            }

            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                if !self.0.is_empty() {
                    self.0.push(' ');
                }
                let _ = write!(self.0, "{}={}", field.name(), value);
            }

            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                if !self.0.is_empty() {
                    self.0.push(' ');
                }
                let _ = write!(self.0, "{}={}", field.name(), value);
            }
        }

        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);

        let log_line = format!("[{}] {}: {}", level, target, visitor.0);

        if let Ok(mut logs) = self.logs.lock() {
            logs.push(log_line);
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Helper to check if logs contain a substring
fn logs_contain(logs: &[String], needle: &str) -> bool {
    logs.iter().any(|log| log.contains(needle))
}

/// Test that `HyperProcess` emits hyperd-starting and hyperd-started events.
#[test]
fn test_hyper_process_logs_startup() {
    let (subscriber, logs) = MockSubscriber::new();
    let _guard = set_default(subscriber);

    let params = common::test_hyper_params("test_hyper_process_logs_startup")
        .expect("Failed to create test parameters");
    let _hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start HyperProcess");

    let logs = logs.lock().unwrap();

    // Verify hyperd-starting event was logged
    assert!(
        logs_contain(&logs, "hyperd-starting"),
        "Expected hyperd-starting log event. Got: {:?}",
        *logs
    );

    // Verify hyperd-started event was logged with endpoint
    assert!(
        logs_contain(&logs, "hyperd-started"),
        "Expected hyperd-started log event. Got: {:?}",
        *logs
    );
    assert!(
        logs_contain(&logs, "endpoint"),
        "Expected endpoint in log. Got: {:?}",
        *logs
    );
}

/// Test that Connection emits connection-parameters event.
#[test]
fn test_connection_logs_parameters() {
    let (subscriber, logs) = MockSubscriber::new();
    let _guard = set_default(subscriber);

    let _test = TestConnection::new().expect("Failed to create test connection");

    let logs = logs.lock().unwrap();

    // Verify connection-parameters event was logged
    assert!(
        logs_contain(&logs, "connection-parameters"),
        "Expected connection-parameters log event. Got: {:?}",
        *logs
    );
    assert!(
        logs_contain(&logs, "host"),
        "Expected host in connection params. Got: {:?}",
        *logs
    );
    assert!(
        logs_contain(&logs, "port"),
        "Expected port in connection params. Got: {:?}",
        *logs
    );
}

/// Test that Connection emits connection-auth-success event.
#[test]
fn test_connection_logs_auth_success() {
    let (subscriber, logs) = MockSubscriber::new();
    let _guard = set_default(subscriber);

    let _test = TestConnection::new().expect("Failed to create test connection");

    let logs = logs.lock().unwrap();

    // Verify authentication success event was logged
    assert!(
        logs_contain(&logs, "connection-auth-success"),
        "Expected connection-auth-success log event. Got: {:?}",
        *logs
    );
}

/// Test that Inserter emits inserter-end event with row count.
#[test]
fn test_inserter_logs_completion() -> Result<()> {
    let (subscriber, logs) = MockSubscriber::new();
    let _guard = set_default(subscriber);

    let test = TestConnection::new().expect("Failed to create test connection");

    // Create a test table
    let table_def = TableDefinition::new("log_test")
        .add_required_column("id", SqlType::int())
        .add_nullable_column("value", SqlType::text());

    test.catalog().create_table(&table_def)?;

    // Insert some rows
    {
        let mut inserter = Inserter::new(&test.connection, &table_def)?;

        for i in 0..100 {
            inserter.add_row(&[&i, &format!("value_{i}")])?;
        }

        let rows = inserter.execute()?;
        assert_eq!(rows, 100);
    }

    let logs = logs.lock().unwrap();

    // Verify inserter-end event was logged with row count
    assert!(
        logs_contain(&logs, "inserter-end"),
        "Expected inserter-end log event. Got: {:?}",
        *logs
    );
    assert!(
        logs_contain(&logs, "rows=100"),
        "Expected rows=100 in log. Got: {:?}",
        *logs
    );
    assert!(
        logs_contain(&logs, "log_test"),
        "Expected table name in log. Got: {:?}",
        *logs
    );

    Ok(())
}

/// Test that multiple operations produce expected log events in sequence.
#[test]
fn test_logging_sequence() -> Result<()> {
    let (subscriber, logs) = MockSubscriber::new();
    let _guard = set_default(subscriber);

    // Start process
    let params = common::test_hyper_params("test_logging_sequence")
        .expect("Failed to create test parameters");
    let hyper = HyperProcess::new(None, Some(&params))?;

    {
        let logs = logs.lock().unwrap();
        assert!(
            logs_contain(&logs, "hyperd-starting"),
            "Expected hyperd-starting. Got: {:?}",
            *logs
        );
        assert!(
            logs_contain(&logs, "hyperd-started"),
            "Expected hyperd-started. Got: {:?}",
            *logs
        );
    }

    // Create connection
    let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("sequence_test.hyper");

    let connection = hyperdb_api::Connection::new(&hyper, &db_path, CreateMode::Create)?;

    {
        let logs = logs.lock().unwrap();
        assert!(
            logs_contain(&logs, "connection-parameters"),
            "Expected connection-parameters. Got: {:?}",
            *logs
        );
    }

    // Create table and insert
    let table_def =
        TableDefinition::new("sequence_table").add_required_column("id", SqlType::int());

    let catalog = hyperdb_api::Catalog::new(&connection);
    catalog.create_table(&table_def)?;

    {
        let mut inserter = Inserter::new(&connection, &table_def)?;
        for i in 0..50 {
            inserter.add_row(&[&i])?;
        }
        inserter.execute()?;
    }

    {
        let logs = logs.lock().unwrap();
        assert!(
            logs_contain(&logs, "inserter-end"),
            "Expected inserter-end. Got: {:?}",
            *logs
        );
        assert!(
            logs_contain(&logs, "rows=50"),
            "Expected rows=50. Got: {:?}",
            *logs
        );
    }

    // Close connection
    connection.close()?;

    Ok(())
}

/// Test that logs don't contain passwords or other sensitive data.
#[test]
fn test_logging_does_not_leak_sensitive_data() {
    let (subscriber, logs) = MockSubscriber::new();
    let _guard = set_default(subscriber);

    // Create a connection (authentication may use passwords internally)
    let _test = TestConnection::new().expect("Failed to create test connection");

    let logs = logs.lock().unwrap();

    // Verify no password-related content in logs
    // Note: "password" might appear in parameter names, but values should be masked
    // We check for common password patterns

    // Should not contain actual credential values
    // This is a sanity check - the connection doesn't use passwords by default,
    // but we want to ensure the logging infrastructure doesn't accidentally
    // include them if they were present.
    assert!(
        !logs_contain(&logs, "secret"),
        "Logs should not contain 'secret'. Got: {:?}",
        *logs
    );
}

/// Test that DEBUG level logs include additional detail.
#[test]
fn test_debug_level_logging() -> Result<()> {
    let (subscriber, logs) = MockSubscriber::new();
    let _guard = set_default(subscriber);

    let test = TestConnection::new().expect("Failed to create test connection");

    // Create a test table
    let table_def = TableDefinition::new("debug_test").add_required_column("id", SqlType::int());

    test.catalog().create_table(&table_def)?;

    // Insert some rows
    {
        let mut inserter = Inserter::new(&test.connection, &table_def)?;

        for i in 0..10 {
            inserter.add_row(&[&i])?;
        }

        inserter.execute()?;
    }

    let logs = logs.lock().unwrap();

    // At DEBUG level, we should see additional connection details
    assert!(
        logs_contain(&logs, "connection-established"),
        "Expected connection-established at DEBUG level. Got: {:?}",
        *logs
    );

    // We should also see the inserter-end event
    assert!(
        logs_contain(&logs, "inserter-end"),
        "Expected inserter-end. Got: {:?}",
        *logs
    );

    Ok(())
}

/// Test logging with connection failure scenario.
#[test]
fn test_logging_connection_failure() {
    let (subscriber, logs) = MockSubscriber::new();
    let _guard = set_default(subscriber);

    let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
    let db_path = temp_dir.path().join("nonexistent.hyper");

    let params = common::test_hyper_params("test_logging_connection_failure")
        .expect("Failed to create test parameters");
    let hyper = HyperProcess::new(None, Some(&params)).expect("Failed to start HyperProcess");

    // Try to open a non-existent database without create mode
    let result = hyperdb_api::Connection::new(&hyper, &db_path, CreateMode::DoNotCreate);

    // Should fail
    assert!(result.is_err());

    let logs = logs.lock().unwrap();

    // The connection attempt was made, so we should see connection-parameters
    assert!(
        logs_contain(&logs, "connection-parameters"),
        "Expected connection-parameters. Got: {:?}",
        *logs
    );
}
