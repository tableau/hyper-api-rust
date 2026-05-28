// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Configuration for Salesforce Data Cloud authentication.

use rsa::pkcs8::DecodePrivateKey;
use rsa::RsaPrivateKey;
use url::Url;
use zeroize::Zeroizing;

use crate::error::{SalesforceAuthError, SalesforceAuthResult};

/// Authentication mode for obtaining an OAuth Access Token from Salesforce.
#[derive(Clone)]
pub enum AuthMode {
    /// Username + password authentication (OAuth password grant).
    ///
    /// Requires `client_secret` to be set in the config.
    Password {
        /// Salesforce username (email)
        username: String,
        /// Salesforce password (may include security token)
        password: Zeroizing<String>,
    },

    /// JWT Bearer Token Flow using RSA private key.
    ///
    /// This is the recommended mode for server-to-server authentication.
    /// Does NOT require `client_secret`.  Each call generates a fresh JWT
    /// assertion, so there is no OAuth Refresh Token to rotate.
    ///
    /// See: <https://help.salesforce.com/s/articleView?id=xcloud.remoteaccess_oauth_jwt_flow.htm>
    PrivateKey {
        /// Salesforce username (email) that authorized the connected app
        username: String,
        /// RSA private key for signing JWT assertions
        private_key: Box<RsaPrivateKey>,
    },

    /// OAuth Refresh Token grant.
    ///
    /// Uses a long-lived OAuth Refresh Token to obtain short-lived OAuth
    /// Access Tokens.  Requires `client_secret` to be set in the config.
    ///
    /// **Important**: The provider caches the OAuth Access Token and only
    /// refreshes it when genuinely expired, to avoid unnecessary OAuth
    /// Refresh Token rotation that would invalidate tokens held by other
    /// connections.
    RefreshToken {
        /// OAuth Refresh Token
        refresh_token: Zeroizing<String>,
    },
}

impl AuthMode {
    /// Creates a password authentication mode.
    pub fn password(username: impl Into<String>, password: impl Into<String>) -> Self {
        AuthMode::Password {
            username: username.into(),
            password: Zeroizing::new(password.into()),
        }
    }

    /// Creates a private key authentication mode from a PEM-encoded private key.
    ///
    /// # Arguments
    ///
    /// * `username` - Salesforce username (email) that authorized the connected app
    /// * `private_key_pem` - RSA private key in PEM format (PKCS#8)
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hyperdb_api_salesforce::AuthMode;
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let pem = "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----";
    /// let mode = AuthMode::private_key("user@example.com", pem)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`SalesforceAuthError::PrivateKey`] if `private_key_pem` is
    /// not a valid PKCS#8 PEM-encoded RSA private key (malformed PEM
    /// envelope, wrong algorithm, or corrupted key bytes).
    pub fn private_key(
        username: impl Into<String>,
        private_key_pem: &str,
    ) -> SalesforceAuthResult<Self> {
        let private_key = RsaPrivateKey::from_pkcs8_pem(private_key_pem).map_err(|e| {
            SalesforceAuthError::private_key(format!(
                "failed to parse private key (expected PKCS#8 PEM format): {e}"
            ))
        })?;

        Ok(AuthMode::PrivateKey {
            username: username.into(),
            private_key: Box::new(private_key),
        })
    }

    /// Creates an OAuth Refresh Token authentication mode.
    pub fn refresh_token(refresh_token: impl Into<String>) -> Self {
        AuthMode::RefreshToken {
            refresh_token: Zeroizing::new(refresh_token.into()),
        }
    }

    /// Returns the username if applicable to this auth mode.
    #[must_use]
    pub fn username(&self) -> Option<&str> {
        match self {
            AuthMode::Password { username, .. } => Some(username),
            AuthMode::PrivateKey { username, .. } => Some(username),
            AuthMode::RefreshToken { .. } => None,
        }
    }
}

impl std::fmt::Debug for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthMode::Password { username, .. } => f
                .debug_struct("Password")
                .field("username", username)
                .field("password", &"[REDACTED]")
                .finish(),
            AuthMode::PrivateKey { username, .. } => f
                .debug_struct("PrivateKey")
                .field("username", username)
                .field("private_key", &"[REDACTED]")
                .finish(),
            AuthMode::RefreshToken { .. } => f
                .debug_struct("RefreshToken")
                .field("refresh_token", &"[REDACTED]")
                .finish(),
        }
    }
}

/// Configuration for the Salesforce Data Cloud token flow.
///
/// Configures how OAuth Access Tokens and DC JWTs are obtained:
/// - `login_url` + `client_id` + `auth_mode` → OAuth Access Token
/// - OAuth Access Token + `dataspace` → DC JWT
///
/// # Example
///
/// ```no_run
/// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode};
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let private_key_pem = "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----";
/// let config = SalesforceAuthConfig::new(
///     "https://login.salesforce.com",
///     "3MVG9...", // Connected App Consumer Key
/// )?
/// .auth_mode(AuthMode::private_key("user@example.com", &private_key_pem)?)
/// .dataspace("default");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SalesforceAuthConfig {
    /// Salesforce login URL (e.g., "<https://login.salesforce.com>" or custom domain)
    pub(crate) login_url: Url,

    /// Connected App Consumer Key (`client_id`)
    pub(crate) client_id: String,

    /// Connected App Consumer Secret (required for Password and `RefreshToken` modes)
    pub(crate) client_secret: Option<Zeroizing<String>>,

    /// Authentication mode (determines how an OAuth Access Token is obtained)
    pub(crate) auth_mode: Option<AuthMode>,

    /// Data Cloud dataspace (sent to `/services/a360/token` when
    /// exchanging the OAuth Access Token for a DC JWT)
    pub(crate) dataspace: Option<String>,

    /// HTTP request timeout in seconds
    pub(crate) timeout_secs: u64,

    /// Maximum number of retries for transient failures
    pub(crate) max_retries: u32,
}

impl SalesforceAuthConfig {
    /// Creates a new configuration with the given login URL and client ID.
    ///
    /// # Arguments
    ///
    /// * `login_url` - Salesforce login URL (e.g., "<https://login.salesforce.com>")
    /// * `client_id` - Connected App Consumer Key
    ///
    /// # Known Login URLs
    ///
    /// - Production: `https://login.salesforce.com`
    /// - Sandbox: `https://test.salesforce.com`
    /// - Custom domain: `https://mydomain.my.salesforce.com`
    ///
    /// # Errors
    ///
    /// Returns [`SalesforceAuthError::Config`] if:
    /// - `login_url` cannot be parsed as a URL (converted from
    ///   [`url::ParseError`]).
    /// - The URL scheme is not `http` or `https`.
    /// - The URL lacks a host component.
    pub fn new(
        login_url: impl AsRef<str>,
        client_id: impl Into<String>,
    ) -> SalesforceAuthResult<Self> {
        let login_url = Url::parse(login_url.as_ref())?;

        // Validate the URL has a scheme and host
        if login_url.scheme() != "https" && login_url.scheme() != "http" {
            return Err(SalesforceAuthError::config(
                "login_url must use http or https scheme",
            ));
        }

        if login_url.host().is_none() {
            return Err(SalesforceAuthError::config("login_url must have a host"));
        }

        Ok(SalesforceAuthConfig {
            login_url,
            client_id: client_id.into(),
            client_secret: None,
            auth_mode: None,
            dataspace: None,
            timeout_secs: 30,
            max_retries: 3,
        })
    }

    /// Sets the authentication mode.
    #[must_use]
    pub fn auth_mode(mut self, mode: AuthMode) -> Self {
        self.auth_mode = Some(mode);
        self
    }

    #[must_use]
    /// Sets the client secret (required for Password and `RefreshToken` modes).
    ///
    /// **Note**: Client secret is NOT required for `PrivateKey` (JWT Bearer) mode.
    pub fn client_secret(mut self, secret: impl Into<String>) -> Self {
        self.client_secret = Some(Zeroizing::new(secret.into()));
        self
    }

    #[must_use]
    /// Sets the Data Cloud dataspace.
    pub fn dataspace(mut self, dataspace: impl Into<String>) -> Self {
        self.dataspace = Some(dataspace.into());
        self
    }

    /// Sets the HTTP request timeout in seconds (default: 30).
    #[must_use]
    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Sets the maximum number of retries for transient failures (default: 3).
    #[must_use]
    pub fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Returns the login URL.
    #[must_use]
    pub fn login_url(&self) -> &Url {
        &self.login_url
    }

    /// Returns the client ID.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Returns the dataspace, if set.
    #[must_use]
    pub fn dataspace_value(&self) -> Option<&str> {
        self.dataspace.as_deref()
    }

    /// Validates the configuration.
    pub(crate) fn validate(&self) -> SalesforceAuthResult<()> {
        let auth_mode = self
            .auth_mode
            .as_ref()
            .ok_or_else(|| SalesforceAuthError::config("auth_mode is required"))?;

        match auth_mode {
            AuthMode::Password { .. } | AuthMode::RefreshToken { .. } => {
                if self.client_secret.is_none() {
                    return Err(SalesforceAuthError::config(
                        "client_secret is required for Password and RefreshToken auth modes",
                    ));
                }
            }
            AuthMode::PrivateKey { .. } => {
                if self.client_secret.is_some() {
                    tracing::warn!(
                        "client_secret is set but not used for PrivateKey (JWT Bearer) mode"
                    );
                }
            }
        }

        Ok(())
    }
}

/// Known Salesforce login URL patterns for validation/warnings.
#[expect(
    dead_code,
    reason = "retained for upcoming login URL warning surface; keep wired up so it stays compiled"
)]
pub(crate) fn is_known_salesforce_host(host: &str) -> bool {
    let patterns = ["login.salesforce.com", "test.salesforce.com"];

    let suffix_patterns = [
        ".my.salesforce.com",
        ".my.site.com",
        ".sandbox.my.salesforce.com",
    ];

    if patterns.contains(&host) {
        return true;
    }

    for suffix in suffix_patterns {
        if host.ends_with(suffix) {
            return true;
        }
    }

    // Test/development patterns
    if host.starts_with("login.test") && host.ends_with(".pc-rnd.salesforce.com") {
        return true;
    }

    false
}
