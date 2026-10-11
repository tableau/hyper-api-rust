// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Connection configuration for Hyper client.

use std::time::Duration;

use super::error::Error;
use super::tls::TlsConfig;

/// Configuration for a Hyper database connection.
///
/// Use the builder pattern to construct a configuration:
///
/// ```no_run
/// // Marked `no_run` to dodge a Windows Defender heuristic that intermittently
/// // refuses to launch this specific compiled doctest binary with
/// // `ERROR_ACCESS_DENIED`. The same builder chain is exercised by
/// // `tests::test_config_builder` so coverage is preserved.
/// use hyperdb_api_core::client::Config;
/// use std::time::Duration;
///
/// let config = Config::new()
///     .with_host("localhost")
///     .with_port(7483)
///     .with_database("test.hyper")
///     .with_user("myuser")
///     .with_password("mypass")
///     .with_connect_timeout(Duration::from_secs(30));
/// ```
#[derive(Debug, Clone)]
#[must_use = "Config uses a consuming builder pattern - each method takes ownership and returns a new instance. You must use the returned value or your configuration changes will be lost"]
pub struct Config {
    host: String,
    port: u16,
    database: Option<String>,
    user: Option<String>,
    password: Option<String>,
    connect_timeout: Option<Duration>,
    application_name: Option<String>,
    options: Vec<(String, String)>,
    tls: TlsConfig,
}

/// libpq connection-string keys that configure TLS. [`Config`]'s `FromStr`
/// rejects them instead of forwarding them to the server as startup options,
/// where they would be silently ignored.
const LIBPQ_TLS_KEYS: &[&str] = &[
    "sslmode",
    "sslrootcert",
    "sslcert",
    "sslkey",
    "sslpassword",
    "sslcrl",
    "sslsni",
    "sslnegotiation",
];

impl Config {
    /// Creates a new configuration with default settings.
    ///
    /// By default, this sets `result_format_code=HyperBinary` for optimal
    /// performance with Hyper's native binary format.
    pub fn new() -> Self {
        Config {
            host: "localhost".to_string(),
            port: 7483,
            database: None,
            user: None,
            password: None,
            connect_timeout: Some(Duration::from_secs(30)),
            application_name: None,
            // Set HyperBinary format by default for optimal performance
            options: vec![("result_format_code".to_string(), "HyperBinary".to_string())],
            tls: TlsConfig::default(),
        }
    }

    /// Sets the host.
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Sets the port.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Sets the database name.
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = Some(database.into());
        self
    }

    /// Sets the username.
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Sets the password.
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Sets the connection timeout.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Sets the application name.
    pub fn with_application_name(mut self, name: impl Into<String>) -> Self {
        self.application_name = Some(name.into());
        self
    }

    /// Adds a custom connection option.
    pub fn with_option(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.push((name.into(), value.into()));
        self
    }

    /// Sets the TLS configuration; the default is
    /// [`TlsMode::Disable`](super::tls::TlsMode::Disable).
    ///
    /// Applies to TCP connections only. Over a Unix domain socket or a named
    /// pipe, [`TlsMode::Prefer`](super::tls::TlsMode::Prefer) connects in
    /// plaintext and every mode that requires TLS is rejected.
    pub fn with_tls(mut self, tls: impl Into<TlsConfig>) -> Self {
        self.tls = tls.into();
        self
    }

    /// Returns the host.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Returns the database name.
    #[must_use]
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// Returns the username.
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Returns the password.
    #[must_use]
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// Returns the connection timeout.
    #[must_use]
    pub fn connect_timeout(&self) -> Option<Duration> {
        self.connect_timeout
    }

    /// Returns the TLS configuration.
    #[must_use]
    pub fn tls(&self) -> &TlsConfig {
        &self.tls
    }

    /// Returns the application name.
    #[must_use]
    pub fn application_name(&self) -> Option<&str> {
        self.application_name.as_deref()
    }

    /// Returns the connection options.
    #[must_use]
    pub fn options(&self) -> &[(String, String)] {
        &self.options
    }

    /// Returns the startup parameters for the connection.
    #[must_use]
    pub fn startup_params(&self) -> Vec<(&str, &str)> {
        let mut params = Vec::new();

        if let Some(ref user) = self.user {
            params.push(("user", user.as_str()));
        }

        if let Some(ref database) = self.database {
            params.push(("database", database.as_str()));
        }

        if let Some(ref app_name) = self.application_name {
            params.push(("application_name", app_name.as_str()));
        }

        // Add custom options
        for (name, value) in &self.options {
            params.push((name.as_str(), value.as_str()));
        }

        params
    }
}

impl std::str::FromStr for Config {
    type Err = Error;

    /// Parses a connection string into a Config.
    ///
    /// Format: `host:port/database?user=xxx&password=xxx`
    ///
    /// The libpq TLS keys (`sslmode`, `sslrootcert`, `sslcert`, `sslkey`, ...)
    /// are rejected with [`Error::Config`]; configure TLS with
    /// [`Config::with_tls`] instead.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut config = Config::new();

        // Parse host:port
        let (addr, rest) = if let Some(idx) = s.find('/') {
            (&s[..idx], &s[idx + 1..])
        } else {
            (s, "")
        };

        if let Some(idx) = addr.rfind(':') {
            config.host = addr[..idx].to_string();
            config.port = addr[idx + 1..]
                .parse()
                .map_err(|_| Error::config("invalid port number"))?;
        } else {
            config.host = addr.to_string();
        }

        // Parse database and query params
        let (database, query) = if let Some(idx) = rest.find('?') {
            (&rest[..idx], &rest[idx + 1..])
        } else {
            (rest, "")
        };

        if !database.is_empty() {
            config.database = Some(database.to_string());
        }

        // Parse query parameters
        for param in query.split('&') {
            if param.is_empty() {
                continue;
            }
            let name = param.split_once('=').map_or(param, |(name, _)| name);
            if LIBPQ_TLS_KEYS.contains(&name) {
                return Err(Error::config(format!(
                    "TLS options are not accepted in the connection string (found `{name}`); \
                     use Config::with_tls (ConnectionBuilder::tls in hyperdb-api)"
                )));
            }
            if let Some(idx) = param.find('=') {
                let value = &param[idx + 1..];
                match name {
                    "user" => config.user = Some(value.to_string()),
                    "password" => config.password = Some(value.to_string()),
                    "application_name" => config.application_name = Some(value.to_string()),
                    _ => config.options.push((name.to_string(), value.to_string())),
                }
            }
        }

        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_str() {
        let config: Config = "localhost:7483/mydb?user=test".parse().unwrap();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 7483);
        assert_eq!(config.database, Some("mydb".to_string()));
        assert_eq!(config.user, Some("test".to_string()));
    }

    #[test]
    fn test_config_from_str_rejects_libpq_tls_keys() {
        for key in LIBPQ_TLS_KEYS {
            let err = format!("h:1/db?{key}=x")
                .parse::<Config>()
                .expect_err("TLS key must be rejected");
            assert!(matches!(err, Error::Config(_)), "{key}: got {err:?}");
            assert!(err.to_string().contains("Config::with_tls"), "got {err}");
        }
        let err = "h:1/db?sslmode=require".parse::<Config>().unwrap_err();
        assert!(err.to_string().contains("sslmode"), "got {err}");
        // A bare key with no `=` is rejected too, not silently dropped.
        let err = "h:1/db?user=u&sslmode".parse::<Config>().unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");

        let err = "h:notaport/db".parse::<Config>().unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");
    }

    /// Mirrors the `///` example on `Config` so the builder chain is still
    /// exercised at runtime even though the doctest itself is `no_run` (see
    /// the doc comment above for why).
    #[test]
    fn test_config_builder() {
        let config = Config::new()
            .with_host("localhost")
            .with_port(7483)
            .with_database("test.hyper")
            .with_user("myuser")
            .with_password("mypass")
            .with_connect_timeout(Duration::from_secs(30));

        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 7483);
        assert_eq!(config.database.as_deref(), Some("test.hyper"));
        assert_eq!(config.user.as_deref(), Some("myuser"));
        assert_eq!(config.password.as_deref(), Some("mypass"));
        assert_eq!(config.connect_timeout, Some(Duration::from_secs(30)));
    }
}
