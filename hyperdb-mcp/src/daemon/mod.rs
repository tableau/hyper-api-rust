// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Single-instance daemon for sharing a `hyperd` process across MCP clients.

pub mod discovery;
pub mod health;
pub mod run;
pub mod spawn;

/// Default TCP port the daemon binds for health checks and single-instance locking.
pub const DEFAULT_DAEMON_PORT: u16 = 7484;

/// Default idle timeout in seconds before the daemon shuts down.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 30 * 60; // 30 minutes

/// Environment variable to override the daemon port.
pub const ENV_DAEMON_PORT: &str = "HYPERDB_DAEMON_PORT";

/// Environment variable to override the idle timeout (seconds).
pub const ENV_IDLE_TIMEOUT: &str = "HYPERDB_DAEMON_IDLE_TIMEOUT";
