// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

use napi::bindgen_prelude::*;
use napi_derive::napi;

use std::collections::HashMap;
use std::path::Path;

use crate::connection::Connection;
use crate::types::CreateMode;

// =============================================================================
// HyperProcess
// =============================================================================

/// Manages a local Hyper server process.
///
/// The server is automatically started when a `HyperProcess` is created and
/// stopped when `close()` is called. You **must** call `close()` when done
/// to ensure the server process is properly terminated.
///
/// @example
/// ```js
/// const hyper = new HyperProcess();
/// const conn = await hyper.connectToDatabase('test.hyper', CreateMode.CreateIfNotExists);
/// // ... use the connection ...
/// await conn.close();
/// hyper.close();
/// ```
#[napi]
#[derive(Debug)]
pub struct HyperProcess {
    inner: Option<hyperdb_api::HyperProcess>,
}

/// Options for starting a [`HyperProcess`].
#[napi(object)]
#[derive(Debug, Clone, Default)]
pub struct HyperProcessOptions {
    /// `"tcp"` (the default) or `"ipc"`: a Unix domain socket, or a named
    /// pipe on Windows. TLS needs `"tcp"`.
    pub transport: Option<String>,
    /// `hyperd` settings, passed through unchanged, for example `ssl_key`
    /// and `ssl_certificate` to serve TLS.
    pub parameters: Option<HashMap<String, String>>,
}

impl TryFrom<&HyperProcessOptions> for hyperdb_api::Parameters {
    type Error = Error;

    fn try_from(options: &HyperProcessOptions) -> Result<Self> {
        let mut params = hyperdb_api::Parameters::new();
        match options.transport.as_deref() {
            None => {}
            Some("tcp") => {
                params.set_transport_mode(hyperdb_api::TransportMode::Tcp);
            }
            Some("ipc") => {
                params.set_transport_mode(hyperdb_api::TransportMode::Ipc);
            }
            Some(other) => {
                return Err(Error::from_reason(format!(
                    "unknown transport `{other}`; expected \"tcp\" or \"ipc\""
                )));
            }
        }
        // Sorted, so the `hyperd` command line does not depend on hash order.
        let mut settings: Vec<_> = options.parameters.iter().flatten().collect();
        settings.sort();
        for (key, value) in settings {
            params.set(key, value);
        }
        Ok(params)
    }
}

#[napi]
impl HyperProcess {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "call-site ergonomics: function consumes logically-owned parameters, refactoring signatures is not worth per-site churn"
    )]
    /// Creates and starts a new Hyper server process.
    ///
    /// The server binary (`hyperd`) is automatically located. Pass a custom
    /// path if it's not in the standard location.
    ///
    /// @param hyperPath - Optional path to the `hyperd` binary.
    /// @param options - Optional transport and `hyperd` settings.
    #[napi(constructor)]
    pub fn new(hyper_path: Option<String>, options: Option<HyperProcessOptions>) -> Result<Self> {
        let path = hyper_path.as_ref().map(|p| Path::new(p.as_str()));
        let params = options
            .as_ref()
            .map(hyperdb_api::Parameters::try_from)
            .transpose()?;
        let process = hyperdb_api::HyperProcess::new(path, params.as_ref())
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(HyperProcess {
            inner: Some(process),
        })
    }

    /// Returns the server endpoint: `host:port` (e.g., "127.0.0.1:7483"),
    /// or with `transport: 'ipc'` the Unix socket path or named pipe.
    ///
    /// Use this to connect to the server via `Connection.connect()`.
    #[napi(getter)]
    pub fn endpoint(&self) -> Result<String> {
        let process = self
            .inner
            .as_ref()
            .ok_or_else(|| Error::from_reason("HyperProcess is closed"))?;
        // Not `process.endpoint()`: over IPC that is the listen descriptor's
        // `<dir>/domain/<name>`, which no connect call accepts.
        process
            .connection_endpoint_string()
            .ok_or_else(|| Error::from_reason("No endpoint available"))
    }

    /// Convenience method: connects to this server with a database.
    ///
    /// @param databasePath - Path to the database file.
    /// @param createMode - How to handle database creation.
    #[napi]
    pub async fn connect_to_database(
        &self,
        database_path: String,
        create_mode: CreateMode,
    ) -> Result<Connection> {
        Connection::connect(self.endpoint()?, database_path, create_mode).await
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "signature retained for API symmetry / future fallibility; returning Result/Option keeps callers from breaking when the function later grows failure cases"
    )]
    /// Stops the Hyper server process.
    ///
    /// This must be called when you're done to ensure proper cleanup.
    /// After calling this, no further connections can be made.
    #[napi]
    pub fn close(&mut self) -> Result<()> {
        // Drop the process, which triggers graceful shutdown
        self.inner.take();
        Ok(())
    }

    /// Returns true if the process is still running.
    #[napi(getter)]
    pub fn is_open(&self) -> bool {
        self.inner.is_some()
    }

    /// Returns the path to the Hyper log file (`hyperd.log`).
    ///
    /// Use this with `conn.enableQueryStats(hyper.logPath)` to enable
    /// detailed query performance statistics collection.
    ///
    /// Returns `null` if the log directory could not be determined.
    #[napi(getter)]
    pub fn log_path(&self) -> Result<Option<String>> {
        let process = self
            .inner
            .as_ref()
            .ok_or_else(|| Error::from_reason("HyperProcess is closed"))?;
        Ok(process
            .log_dir()
            .map(|dir| dir.join("hyperd.log").to_string_lossy().to_string()))
    }
}
