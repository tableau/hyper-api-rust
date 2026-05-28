// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! DC JWT provider for Salesforce Data Cloud authentication.
//!
//! This module implements the two-stage token flow:
//! 1. Authenticate with Salesforce to get an **OAuth Access Token**
//!    (via `/services/oauth2/token`)
//! 2. Exchange the OAuth Access Token for a **DC JWT**
//!    (via `/services/a360/token`)
//!
//! The provider caches both the OAuth Access Token and the DC JWT
//! independently.  The OAuth Access Token is only refreshed when it
//! has genuinely expired, avoiding unnecessary **OAuth Refresh Token**
//! rotation that would invalidate tokens held by other connections.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client as HttpClient;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::config::{AuthMode, SalesforceAuthConfig};
use crate::error::{SalesforceAuthError, SalesforceAuthResult};
use crate::jwt::build_jwt_assertion;
use crate::token::{DataCloudToken, DataCloudTokenResponse, OAuthToken, OAuthTokenResponse};

/// OAuth Access Token endpoint path.
const OAUTH_TOKEN_PATH: &str = "services/oauth2/token";

/// DC JWT exchange endpoint path.
const DATA_CLOUD_TOKEN_PATH: &str = "services/a360/token";

/// DC JWT provider.
///
/// Handles the full token flow for Salesforce Data Cloud:
/// 1. Authenticates with Salesforce using the configured auth mode to
///    obtain an **OAuth Access Token**
/// 2. Exchanges the OAuth Access Token for a **DC JWT**
/// 3. Caches both tokens and refreshes them independently:
///    - The OAuth Access Token is refreshed only when genuinely expired
///      (to avoid unnecessary OAuth Refresh Token rotation)
///    - The DC JWT is refreshed whenever it is expired or requested
///
/// On DC JWT exchange failure, the provider retries once with a
/// force-refreshed OAuth Access Token (Step 2a), matching the behavior
/// described in the `GenieOAuthManagement` documentation.
///
/// # Example
///
/// ```no_run
/// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, DataCloudTokenProvider};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let private_key_pem = "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----";
/// let config = SalesforceAuthConfig::new(
///     "https://login.salesforce.com",
///     "your-client-id",
/// )?
/// .auth_mode(AuthMode::private_key("user@example.com", &private_key_pem)?);
///
/// let mut provider = DataCloudTokenProvider::new(config)?;
///
/// // Get a valid DC JWT (automatically handles the full token flow)
/// let token = provider.get_token().await?;
/// println!("Authorization: {}", token.bearer_token());
/// # Ok(())
/// # }
/// ```
pub struct DataCloudTokenProvider {
    /// Configuration
    config: SalesforceAuthConfig,
    /// HTTP client for token requests
    http_client: HttpClient,
    /// Cached OAuth Access Token (refreshed only when genuinely expired)
    cached_oauth_token: Option<OAuthToken>,
    /// Cached DC JWT
    cached_dc_jwt: Option<DataCloudToken>,
}

impl DataCloudTokenProvider {
    /// Creates a new DC JWT provider with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration is invalid.
    pub fn new(config: SalesforceAuthConfig) -> SalesforceAuthResult<Self> {
        config.validate()?;

        let http_client = HttpClient::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|e| SalesforceAuthError::http(format!("failed to create HTTP client: {e}")))?;

        Ok(DataCloudTokenProvider {
            config,
            http_client,
            cached_oauth_token: None,
            cached_dc_jwt: None,
        })
    }

    /// Returns the configuration.
    #[must_use]
    pub fn config(&self) -> &SalesforceAuthConfig {
        &self.config
    }

    /// Gets a valid DC JWT.
    ///
    /// If a cached DC JWT exists and is still valid, it is returned.
    /// Otherwise, a new DC JWT is obtained through the full token flow.
    ///
    /// # Errors
    ///
    /// Propagates any error from `Self::fetch_dc_jwt` — typically
    /// [`SalesforceAuthError::Http`], [`SalesforceAuthError::Authorization`],
    /// [`SalesforceAuthError::Jwt`], [`SalesforceAuthError::TokenExchange`],
    /// or [`SalesforceAuthError::TokenParse`] depending on where the
    /// three-step refresh cycle (OAuth Access Token → DC JWT) fails.
    ///
    /// # Panics
    ///
    /// Does not panic in practice. The trailing `unwrap()` on
    /// `self.cached_dc_jwt` is guarded by the preceding cache-population
    /// logic: either the cache was already populated with a valid token,
    /// or `Self::fetch_dc_jwt` just filled it.
    pub async fn get_token(&mut self) -> SalesforceAuthResult<&DataCloudToken> {
        let needs_refresh = match &self.cached_dc_jwt {
            Some(token) if token.is_valid() => {
                debug!("Using cached DC JWT");
                false
            }
            Some(_) => {
                debug!("Cached DC JWT expired, refreshing");
                true
            }
            None => true,
        };

        if needs_refresh {
            let token = self.fetch_dc_jwt().await?;
            self.cached_dc_jwt = Some(token);
        }

        Ok(self.cached_dc_jwt.as_ref().unwrap())
    }

    /// Forces a full token refresh (both OAuth Access Token and DC JWT),
    /// even if the cached tokens are still valid.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::get_token`] (same failure modes
    /// as the full token-flow refresh).
    pub async fn force_refresh(&mut self) -> SalesforceAuthResult<&DataCloudToken> {
        self.cached_oauth_token = None;
        self.cached_dc_jwt = None;
        self.get_token().await
    }

    /// Forces a DC JWT refresh while allowing the OAuth Access Token to
    /// be reused if still valid.
    ///
    /// This is the preferred refresh method during normal operation: it
    /// re-exchanges the (possibly cached) OAuth Access Token for a fresh
    /// DC JWT without unnecessarily rotating the OAuth Refresh Token.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`Self::get_token`] (HTTP, authorization,
    /// JWT signing, or token-parse failures during the DC JWT exchange).
    pub async fn refresh_token(&mut self) -> SalesforceAuthResult<&DataCloudToken> {
        self.cached_dc_jwt = None;
        self.get_token().await
    }

    /// Clears all cached tokens (both OAuth Access Token and DC JWT).
    pub fn clear_cache(&mut self) {
        self.cached_oauth_token = None;
        self.cached_dc_jwt = None;
    }

    /// Returns the DC JWT bearer token string if a valid DC JWT is cached.
    ///
    /// Convenience method for getting the `Authorization` header value
    /// without an async call.  Returns `None` if no valid DC JWT is cached.
    #[must_use]
    pub fn bearer_token(&self) -> Option<String> {
        self.cached_dc_jwt
            .as_ref()
            .filter(|t| t.is_valid())
            .map(super::token::DataCloudToken::bearer_token)
    }

    /// Returns the tenant URL if a valid DC JWT is cached.
    #[must_use]
    pub fn tenant_url(&self) -> Option<&str> {
        self.cached_dc_jwt
            .as_ref()
            .filter(|t| t.is_valid())
            .map(super::token::DataCloudToken::tenant_url_str)
    }

    /// Returns the lakehouse name for Hyper connection.
    ///
    /// # Errors
    ///
    /// Propagates [`SalesforceAuthError::TokenParse`] from
    /// [`DataCloudToken::lakehouse_name`] if the cached DC JWT's tenant
    /// URL cannot be parsed into a valid lakehouse identifier.
    pub fn lakehouse_name(&self) -> SalesforceAuthResult<Option<String>> {
        if let Some(ref token) = self.cached_dc_jwt {
            if token.is_valid() {
                return Ok(Some(token.lakehouse_name(self.config.dataspace_value())?));
            }
        }
        Ok(None)
    }

    /// Fetches a new DC JWT through the full token flow.
    ///
    /// Implements the three-step refresh cycle from the
    /// `GenieOAuthManagement` documentation:
    ///
    /// - **Step 1**: Validate / refresh the OAuth Access Token
    ///   (only refreshes when genuinely expired — avoids unnecessary
    ///   OAuth Refresh Token rotation)
    /// - **Step 2**: Exchange the OAuth Access Token for a DC JWT
    /// - **Step 2a** (retry): If Step 2 fails, force-refresh the
    ///   OAuth Access Token and retry the DC JWT exchange once
    async fn fetch_dc_jwt(&mut self) -> SalesforceAuthResult<DataCloudToken> {
        // Step 1: Validate / refresh OAuth Access Token
        let oauth_token = self.get_valid_oauth_access_token().await?;

        // Step 2: Exchange OAuth Access Token → DC JWT
        match self
            .exchange_oauth_access_token_for_dc_jwt(&oauth_token)
            .await
        {
            Ok(dc_jwt) => Ok(dc_jwt),
            Err(step2_err) => {
                // Step 2a: Force-refresh the OAuth Access Token and retry once.
                // This handles the case where the OAuth Access Token appeared
                // valid locally but was invalidated server-side (e.g., by
                // Salesforce's inactivity timeout).
                warn!(
                    error = %step2_err,
                    "DC JWT exchange failed; force-refreshing OAuth Access Token and retrying (Step 2a)"
                );

                self.cached_oauth_token = None;
                let fresh_oauth_token = self.fetch_oauth_access_token().await?;
                self.cached_oauth_token = Some(fresh_oauth_token.clone());

                self.exchange_oauth_access_token_for_dc_jwt(&fresh_oauth_token)
                    .await
                    .map_err(|retry_err| {
                        warn!(
                            original_error = %step2_err,
                            retry_error = %retry_err,
                            "DC JWT exchange failed again after OAuth Access Token refresh (Step 2a retry)"
                        );
                        retry_err
                    })
            }
        }
    }

    /// Returns a valid OAuth Access Token, using the cache when possible.
    ///
    /// Only contacts Salesforce when the cached OAuth Access Token has
    /// genuinely expired.  This avoids unnecessary OAuth Refresh Token
    /// rotation that would invalidate tokens held by other connections.
    async fn get_valid_oauth_access_token(&mut self) -> SalesforceAuthResult<OAuthToken> {
        if let Some(ref token) = self.cached_oauth_token {
            if token.is_likely_valid() {
                debug!(
                    "OAuth Access Token still valid (obtained at {}), reusing",
                    token.obtained_at
                );
                return Ok(token.clone());
            }
            debug!("Cached OAuth Access Token expired, refreshing");
        }

        let token = self.fetch_oauth_access_token().await?;
        self.cached_oauth_token = Some(token.clone());
        Ok(token)
    }

    /// Fetches a fresh OAuth Access Token from Salesforce.
    async fn fetch_oauth_access_token(&self) -> SalesforceAuthResult<OAuthToken> {
        let auth_mode = self
            .config
            .auth_mode
            .as_ref()
            .ok_or_else(|| SalesforceAuthError::config("auth_mode not configured"))?;

        let mut form_data = HashMap::new();
        form_data.insert("client_id", self.config.client_id.clone());

        match auth_mode {
            AuthMode::Password { username, password } => {
                info!(username = %username, "Fetching OAuth Access Token via password grant");
                form_data.insert("grant_type", "password".to_string());
                form_data.insert("username", username.clone());
                form_data.insert("password", password.as_str().to_string());

                if let Some(ref secret) = self.config.client_secret {
                    form_data.insert("client_secret", secret.as_str().to_string());
                }
            }

            AuthMode::PrivateKey {
                username,
                private_key,
            } => {
                info!(username = %username, "Fetching OAuth Access Token via JWT Bearer Token Flow");

                let assertion = build_jwt_assertion(
                    &self.config.client_id,
                    username,
                    &self.config.login_url,
                    private_key,
                )?;

                form_data.insert(
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string(),
                );
                form_data.insert("assertion", assertion);
            }

            AuthMode::RefreshToken { refresh_token } => {
                info!("Fetching OAuth Access Token via OAuth Refresh Token");
                form_data.insert("grant_type", "refresh_token".to_string());
                form_data.insert("refresh_token", refresh_token.as_str().to_string());

                if let Some(ref secret) = self.config.client_secret {
                    form_data.insert("client_secret", secret.as_str().to_string());
                }
            }
        }

        let token_url = self.config.login_url.join(OAUTH_TOKEN_PATH).map_err(|e| {
            SalesforceAuthError::config(format!("failed to build OAuth Access Token URL: {e}"))
        })?;

        debug!(url = %token_url, "Requesting OAuth Access Token");

        let response = self.post_with_retry(&token_url, &form_data).await?;
        let response_text = response.text().await?;

        debug!(response = %response_text, "OAuth Access Token response received");

        let oauth_response: OAuthTokenResponse =
            serde_json::from_str(&response_text).map_err(|e| {
                SalesforceAuthError::token_parse(format!(
                    "failed to parse OAuth Access Token response: {e}"
                ))
            })?;

        let token_changed = self
            .cached_oauth_token
            .as_ref()
            .map_or(true, |old| old.token != oauth_response.access_token);

        debug!(
            instance_url = %oauth_response.instance_url,
            token_type = ?oauth_response.token_type,
            scope = ?oauth_response.scope,
            token_changed = token_changed,
            "OAuth Access Token response parsed"
        );

        OAuthToken::from_response(oauth_response)
    }

    /// Exchanges an OAuth Access Token for a DC JWT.
    ///
    /// Calls `POST /services/a360/token` with the OAuth Access Token as
    /// the `subject_token`.
    async fn exchange_oauth_access_token_for_dc_jwt(
        &self,
        oauth_token: &OAuthToken,
    ) -> SalesforceAuthResult<DataCloudToken> {
        let mut form_data = HashMap::new();
        form_data.insert(
            "grant_type",
            "urn:salesforce:grant-type:external:cdp".to_string(),
        );
        form_data.insert(
            "subject_token_type",
            "urn:ietf:params:oauth:token-type:access_token".to_string(),
        );
        form_data.insert("subject_token", oauth_token.token.clone());

        if let Some(ref dataspace) = self.config.dataspace {
            form_data.insert("dataspace", dataspace.clone());
        }

        let exchange_url = oauth_token
            .instance_url
            .join(DATA_CLOUD_TOKEN_PATH)
            .map_err(|e| {
                SalesforceAuthError::config(format!("failed to build DC JWT exchange URL: {e}"))
            })?;

        debug!(url = %exchange_url, "Exchanging OAuth Access Token for DC JWT");

        let response = self.post_with_retry(&exchange_url, &form_data).await?;
        let response_text = response.text().await?;

        debug!(response = %response_text, "DC JWT response received");

        let dc_response: DataCloudTokenResponse =
            serde_json::from_str(&response_text).map_err(|e| {
                SalesforceAuthError::token_parse(format!("failed to parse DC JWT response: {e}"))
            })?;

        debug!(
            instance_url = %dc_response.instance_url,
            token_type = ?dc_response.token_type,
            expires_in = ?dc_response.expires_in,
            "DC JWT response parsed"
        );

        let token = DataCloudToken::from_response(dc_response)?;

        info!(
            tenant_url = %token.tenant_url(),
            expires_at = %token.expires_at(),
            "DC JWT obtained"
        );

        Ok(token)
    }

    /// Makes a POST request with retry logic for transient failures.
    async fn post_with_retry(
        &self,
        url: &url::Url,
        form_data: &HashMap<&str, String>,
    ) -> SalesforceAuthResult<reqwest::Response> {
        let mut last_error = None;

        for attempt in 0..=self.config.max_retries {
            if attempt > 0 {
                let delay = Duration::from_secs(1 << (attempt - 1).min(4));
                warn!(
                    attempt = attempt,
                    delay_secs = delay.as_secs(),
                    "Retrying after transient failure"
                );
                tokio::time::sleep(delay).await;
            }

            match self
                .http_client
                .post(url.as_str())
                .header("Accept", "application/json")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .form(form_data)
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_client_error() {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();

                        if let Ok(error_json) = serde_json::from_str::<serde_json::Value>(&body) {
                            let error_code = error_json
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let error_desc = error_json
                                .get("error_description")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&body);

                            return Err(SalesforceAuthError::authorization(
                                error_code.to_string(),
                                error_desc.to_string(),
                            ));
                        }

                        return Err(SalesforceAuthError::http(format!(
                            "HTTP {status} error: {body}"
                        )));
                    }

                    if response.status().is_server_error() {
                        last_error = Some(SalesforceAuthError::http(format!(
                            "HTTP {} error",
                            response.status()
                        )));
                        continue;
                    }

                    return Ok(response);
                }
                Err(e) => {
                    last_error = Some(SalesforceAuthError::Http(e.to_string()));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| SalesforceAuthError::http("request failed after retries")))
    }
}

/// Thread-safe wrapper around [`DataCloudTokenProvider`].
///
/// Allows sharing the DC JWT provider between multiple tasks/threads
/// while ensuring exclusive access during token operations.  All access
/// is protected by a [`tokio::sync::Mutex`].
///
/// # Example
///
/// ```no_run
/// use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let config = SalesforceAuthConfig::new("https://login.salesforce.com", "client_id")?
/// #     .auth_mode(AuthMode::password("user", "pass"));
/// let provider = SharedTokenProvider::new(config)?;
///
/// // Can be cloned and shared between tasks
/// let provider_clone = provider.clone();
///
/// tokio::spawn(async move {
///     let dc_jwt = provider_clone.get_token().await.unwrap();
///     // use dc_jwt.bearer_token() as the Authorization header
/// });
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SharedTokenProvider {
    inner: Arc<Mutex<DataCloudTokenProvider>>,
}

impl SharedTokenProvider {
    /// Creates a new shared DC JWT provider.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`DataCloudTokenProvider::new`]:
    /// configuration validation failures or HTTP client construction
    /// failures (surfaced as [`SalesforceAuthError::Http`]).
    pub fn new(config: SalesforceAuthConfig) -> SalesforceAuthResult<Self> {
        let provider = DataCloudTokenProvider::new(config)?;
        Ok(SharedTokenProvider {
            inner: Arc::new(Mutex::new(provider)),
        })
    }

    /// Gets a valid DC JWT.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`DataCloudTokenProvider::get_token`]
    /// (HTTP failure, authorization rejection, JWT signing error, or
    /// token-parse failure during the refresh cycle).
    pub async fn get_token(&self) -> SalesforceAuthResult<DataCloudToken> {
        let mut provider = self.inner.lock().await;
        provider.get_token().await.cloned()
    }

    /// Forces a DC JWT refresh (reuses OAuth Access Token if still valid).
    ///
    /// # Errors
    ///
    /// Propagates any error from [`DataCloudTokenProvider::refresh_token`].
    pub async fn refresh_token(&self) -> SalesforceAuthResult<DataCloudToken> {
        let mut provider = self.inner.lock().await;
        provider.refresh_token().await.cloned()
    }

    /// Forces a full refresh (both OAuth Access Token and DC JWT).
    ///
    /// # Errors
    ///
    /// Propagates any error from [`DataCloudTokenProvider::force_refresh`].
    pub async fn force_refresh(&self) -> SalesforceAuthResult<DataCloudToken> {
        let mut provider = self.inner.lock().await;
        provider.force_refresh().await.cloned()
    }

    /// Returns the DC JWT bearer token string if a valid DC JWT is cached.
    pub async fn bearer_token(&self) -> Option<String> {
        let provider = self.inner.lock().await;
        provider.bearer_token()
    }

    /// Returns the tenant URL if a valid DC JWT is cached.
    pub async fn tenant_url(&self) -> Option<String> {
        let provider = self.inner.lock().await;
        provider.tenant_url().map(std::string::ToString::to_string)
    }

    /// Returns the lakehouse name for Hyper connection.
    ///
    /// # Errors
    ///
    /// Propagates [`SalesforceAuthError::TokenParse`] from
    /// [`DataCloudTokenProvider::lakehouse_name`] if the cached DC JWT's
    /// tenant URL cannot be parsed into a valid lakehouse identifier.
    pub async fn lakehouse_name(&self) -> SalesforceAuthResult<Option<String>> {
        let provider = self.inner.lock().await;
        provider.lakehouse_name()
    }
}

impl std::fmt::Debug for DataCloudTokenProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataCloudTokenProvider")
            .field("config", &self.config)
            .field("has_cached_oauth_token", &self.cached_oauth_token.is_some())
            .field("has_cached_dc_jwt", &self.cached_dc_jwt.is_some())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SharedTokenProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedTokenProvider")
            .finish_non_exhaustive()
    }
}
