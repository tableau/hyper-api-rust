// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Single-instance daemon for sharing a `hyperd` process across MCP clients.

pub mod discovery;
pub mod health;
pub mod run;
pub mod spawn;

/// Default base TCP port for the daemon health listener. When no env var is set,
/// the daemon scans `[base, base + DAEMON_PORT_SCAN_SPAN)` to find a free port.
/// Previously 7484; changed to 7485 to avoid collision with hyperd's default gRPC port.
pub const DEFAULT_DAEMON_BASE_PORT: u16 = 7485;

/// Number of ports to scan starting from the base port when discovering or spawning
/// a daemon. Used by the later port-scanning stage (not yet implemented).
pub const DAEMON_PORT_SCAN_SPAN: u16 = 16;

/// Suggested idle timeout value (30 minutes) for use with the `--idle-timeout` flag
/// or `HYPERDB_DAEMON_IDLE_TIMEOUT` env var. By default (when neither is set), the
/// daemon never auto-shuts down due to inactivity.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 30 * 60;

/// Environment variable to override the daemon port.
pub const ENV_DAEMON_PORT: &str = "HYPERDB_DAEMON_PORT";

/// Environment variable to override the idle timeout (seconds).
pub const ENV_IDLE_TIMEOUT: &str = "HYPERDB_DAEMON_IDLE_TIMEOUT";
