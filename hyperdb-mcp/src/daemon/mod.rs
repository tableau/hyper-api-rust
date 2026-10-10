// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Single-instance daemon for sharing a `hyperd` process across MCP clients.
//!
//! A daemon is identified by its state directory (`~/.hyperdb`, or
//! `HYPERDB_STATE_DIR`): the single-instance lock is `daemon.lock` in it, the
//! health channel is a per-user Unix socket or named pipe (see [`control`]), and
//! `daemon.json` tells clients where to find both. There is no TCP port.

pub mod control;
pub mod discovery;
pub mod health;
pub mod lock;
pub mod run;
pub mod spawn;
pub mod state_perms;

/// Suggested idle timeout value (30 minutes) for use with the `--idle-timeout` flag
/// or `HYPERDB_DAEMON_IDLE_TIMEOUT` env var. By default (when neither is set), the
/// daemon never auto-shuts down due to inactivity.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 30 * 60;

/// Environment variable to override the idle timeout (seconds).
pub const ENV_IDLE_TIMEOUT: &str = "HYPERDB_DAEMON_IDLE_TIMEOUT";
