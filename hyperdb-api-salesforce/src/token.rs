// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Token types for Salesforce Data Cloud authentication.
//!
//! This module defines the three token types used in the Salesforce Data Cloud
//! authentication flow:
//!
//! 1. **OAuth Refresh Token** → used to obtain an OAuth Access Token (not modeled here;
//!    it is a configuration input via [`AuthMode::RefreshToken`](super::config::AuthMode))
//! 2. **OAuth Access Token** ([`OAuthToken`]) → obtained from Salesforce `/services/oauth2/token`
//! 3. **DC JWT** ([`DataCloudToken`]) → obtained by exchanging the OAuth Access Token
//!    at `/services/a360/token`, sent as `Authorization` header with every gRPC call

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use url::Url;

use crate::error::{SalesforceAuthError, SalesforceAuthResult};

/// Default validity buffer for [`DataCloudToken::is_valid`].
///
/// A DC JWT is considered invalid when it has fewer than this many seconds
/// of remaining lifetime. This provides a safety margin so callers never
/// use a token that is about to expire.
const DC_JWT_VALIDITY_BUFFER_SECS: i64 = 300;

/// OAuth Access Token response from Salesforce `/services/oauth2/token`.
///
/// See: <https://help.salesforce.com/s/articleView?id=sf.remoteaccess_oauth_jwt_flow.htm>
#[derive(Debug, Deserialize)]
pub struct OAuthTokenResponse {
    /// OAuth Access Token
    pub access_token: String,

    /// Salesforce instance URL (e.g., "<https://na1.salesforce.com>")
    pub instance_url: String,

    /// Token type (usually "Bearer")
    #[serde(default)]
    pub token_type: Option<String>,

    /// Token scope
    #[serde(default)]
    pub scope: Option<String>,

    /// When the OAuth Access Token was issued (Unix timestamp in milliseconds)
    #[serde(default)]
    pub issued_at: Option<String>,

    /// Error code (present on failure)
    #[serde(default)]
    pub error: Option<String>,

    /// Error description (present on failure)
    #[serde(default)]
    pub error_description: Option<String>,
}

impl OAuthTokenResponse {
    /// Checks if the response contains an error.
    pub fn check_error(&self) -> SalesforceAuthResult<()> {
        if let (Some(code), Some(desc)) = (&self.error, &self.error_description) {
            return Err(SalesforceAuthError::authorization(
                code.clone(),
                desc.clone(),
            ));
        }
        if self.access_token.is_empty() {
            return Err(SalesforceAuthError::token_parse(
                "missing access_token in OAuth Access Token response",
            ));
        }
        Ok(())
    }
}

/// Parsed OAuth Access Token with Salesforce instance URL.
///
/// Obtained from `/services/oauth2/token`. This token is exchanged for a
/// DC JWT via `/services/a360/token`.
#[derive(Debug, Clone)]
pub struct OAuthToken {
    /// OAuth Access Token value
    pub token: String,
    /// Salesforce instance URL (used as base URL for the DC JWT exchange)
    pub instance_url: Url,
    /// When this OAuth Access Token was obtained
    pub obtained_at: DateTime<Utc>,
    /// Estimated expiry (Salesforce reports ~2 hours, but server-side
    /// inactivity timeout can invalidate it earlier)
    pub expires_at: DateTime<Utc>,
}

/// Default OAuth Access Token lifetime in seconds.
///
/// Salesforce reports `access-token-expires-in: 7199` (~2 hours), but the
/// server-side session can be invalidated earlier by the org's inactivity
/// timeout (commonly 15 min – 2 hours).
const OAUTH_ACCESS_TOKEN_DEFAULT_LIFETIME_SECS: i64 = 7199;

impl OAuthToken {
    /// Creates an OAuth Access Token from a response.
    ///
    /// # Errors
    ///
    /// - Returns [`SalesforceAuthError::Authorization`] if `response`
    ///   carries both `error` and `error_description` fields (via
    ///   `OAuthTokenResponse::check_error`).
    /// - Returns [`SalesforceAuthError::TokenParse`] if `response.access_token`
    ///   is empty, or if `response.instance_url` cannot be parsed as a URL.
    pub fn from_response(response: OAuthTokenResponse) -> SalesforceAuthResult<Self> {
        response.check_error()?;

        let instance_url = Url::parse(&response.instance_url)
            .map_err(|e| SalesforceAuthError::token_parse(format!("invalid instance_url: {e}")))?;

        let now = Utc::now();
        let expires_at = now + Duration::seconds(OAUTH_ACCESS_TOKEN_DEFAULT_LIFETIME_SECS);

        Ok(OAuthToken {
            token: response.access_token,
            instance_url,
            obtained_at: now,
            expires_at,
        })
    }

    /// Returns the bearer token string (e.g., "Bearer abc123...").
    #[must_use]
    pub fn bearer_token(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Returns `true` if the OAuth Access Token has not yet reached its
    /// estimated expiry time.
    #[must_use]
    pub fn is_likely_valid(&self) -> bool {
        Utc::now() < self.expires_at
    }
}

/// DC JWT response from `/services/a360/token`.
///
/// See: <https://developer.salesforce.com/docs/atlas.en-us.c360a_api.meta/c360a_api/c360a_getting_started_with_cdp.htm>
#[derive(Debug, Deserialize)]
pub struct DataCloudTokenResponse {
    /// DC JWT value
    pub access_token: String,

    /// Data Cloud instance URL (tenant URL)
    pub instance_url: String,

    /// Token type (usually "Bearer")
    #[serde(default)]
    pub token_type: Option<String>,

    /// DC JWT expiration time in seconds
    #[serde(default)]
    pub expires_in: Option<i64>,

    /// Error code (present on failure)
    #[serde(default)]
    pub error: Option<String>,

    /// Error description (present on failure)
    #[serde(default)]
    pub error_description: Option<String>,
}

impl DataCloudTokenResponse {
    /// Checks if the response contains an error.
    pub fn check_error(&self) -> SalesforceAuthResult<()> {
        if let (Some(code), Some(desc)) = (&self.error, &self.error_description) {
            return Err(SalesforceAuthError::authorization(
                code.clone(),
                desc.clone(),
            ));
        }
        if self.access_token.is_empty() {
            return Err(SalesforceAuthError::token_parse(
                "missing access_token in DC JWT response",
            ));
        }
        Ok(())
    }
}

/// Data Cloud JWT (DC JWT) for Hyper gRPC authentication.
///
/// Obtained by exchanging an OAuth Access Token at `/services/a360/token`.
/// Sent as the `Authorization: Bearer <jwt>` header with every gRPC call
/// to the Hyper query engine.
///
/// The DC JWT has a ~2-hour lifetime (`exp` claim), but is proactively
/// refreshed much earlier (every ~15 minutes by default) so that the
/// underlying OAuth Access Token is revalidated before Salesforce's
/// server-side inactivity timeout can invalidate it.
#[derive(Debug, Clone)]
pub struct DataCloudToken {
    /// Token type (e.g., "Bearer")
    token_type: String,
    /// DC JWT value
    token: String,
    /// Data Cloud tenant URL
    tenant_url: Url,
    /// When this DC JWT was obtained (used for maxAge-based proactive refresh)
    created_at: DateTime<Utc>,
    /// DC JWT expiration time (from `expires_in` in the response)
    expires_at: DateTime<Utc>,
}

impl DataCloudToken {
    /// Creates a DC JWT from a `/services/a360/token` response.
    ///
    /// # Errors
    ///
    /// - Returns [`SalesforceAuthError::Authorization`] if `response`
    ///   carries both `error` and `error_description` fields.
    /// - Returns [`SalesforceAuthError::TokenParse`] if `response.access_token`
    ///   is empty, or if `response.instance_url` cannot be parsed as a URL
    ///   (after prepending `https://` when the scheme is missing).
    pub fn from_response(response: DataCloudTokenResponse) -> SalesforceAuthResult<Self> {
        response.check_error()?;

        let instance_url_with_scheme = if response.instance_url.starts_with("http://")
            || response.instance_url.starts_with("https://")
        {
            response.instance_url.clone()
        } else {
            format!("https://{}", response.instance_url)
        };

        let tenant_url = Url::parse(&instance_url_with_scheme)
            .map_err(|e| SalesforceAuthError::token_parse(format!("invalid instance_url: {e}")))?;

        let token_type = response.token_type.unwrap_or_else(|| "Bearer".to_string());

        let now = Utc::now();
        // Default to 30 minutes if Salesforce doesn't report expires_in
        let expires_in_secs = response.expires_in.unwrap_or(1800);
        let expires_at = now + Duration::seconds(expires_in_secs);

        Ok(DataCloudToken {
            token_type,
            token: response.access_token,
            tenant_url,
            created_at: now,
            expires_at,
        })
    }

    /// Returns the bearer token string for the `Authorization` header.
    ///
    /// Format: `"Bearer <dc_jwt>"`
    #[must_use]
    pub fn bearer_token(&self) -> String {
        format!("{} {}", self.token_type, self.token)
    }

    /// Returns just the DC JWT value (without the type prefix).
    #[must_use]
    pub fn access_token(&self) -> &str {
        &self.token
    }

    /// Returns the token type (e.g., "Bearer").
    #[must_use]
    pub fn token_type(&self) -> &str {
        &self.token_type
    }

    /// Returns the Data Cloud tenant URL.
    #[must_use]
    pub fn tenant_url(&self) -> &Url {
        &self.tenant_url
    }

    /// Returns the tenant URL as a string (for the `audience` gRPC header).
    #[must_use]
    pub fn tenant_url_str(&self) -> &str {
        self.tenant_url.as_str()
    }

    /// Returns when this DC JWT was obtained.
    #[must_use]
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    /// Returns the DC JWT expiration time.
    #[must_use]
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    /// Returns the age of this DC JWT (time since it was obtained).
    #[must_use]
    pub fn age(&self) -> Duration {
        Utc::now().signed_duration_since(self.created_at)
    }

    /// Returns the remaining lifetime of this DC JWT.
    #[must_use]
    pub fn remaining_lifetime(&self) -> Duration {
        self.expires_at.signed_duration_since(Utc::now())
    }

    /// Checks if the DC JWT is still valid (not expired).
    ///
    /// Returns `true` if the DC JWT has at least 300 seconds (5 minutes) of
    /// remaining lifetime. This buffer ensures callers never use a DC JWT
    /// that is about to expire.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.expires_at > Utc::now() + Duration::seconds(DC_JWT_VALIDITY_BUFFER_SECS)
    }

    /// Checks if the DC JWT is expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.expires_at <= Utc::now()
    }

    /// Checks if the DC JWT should be proactively refreshed.
    ///
    /// Mirrors the C++ `IsDCJWTExpiringSoon` logic: returns `true` when
    /// either the DC JWT is near its hard expiry OR it has exceeded the
    /// maximum age. This ensures:
    /// - The OAuth Access Token is revalidated regularly (catching
    ///   server-side inactivity timeouts)
    /// - The DC JWT is replaced well before its ~2-hour hard expiry
    ///
    /// # Arguments
    /// * `threshold_secs` - Refresh when the DC JWT has fewer than this
    ///   many seconds remaining (default: 300 = 5 minutes)
    /// * `max_age_secs` - Refresh when the DC JWT is older than this
    ///   many seconds (default: 900 = 15 minutes)
    #[must_use]
    pub fn needs_refresh(&self, threshold_secs: i64, max_age_secs: i64) -> bool {
        let now = Utc::now();
        let expiring = (self.expires_at - now).num_seconds() <= threshold_secs;
        let too_old = (now - self.created_at).num_seconds() > max_age_secs;
        expiring || too_old
    }

    /// Extracts the tenant ID from the DC JWT payload.
    ///
    /// The tenant ID is stored in the `audienceTenantId` claim of the JWT.
    ///
    /// # Errors
    ///
    /// Returns [`SalesforceAuthError::TokenParse`] if:
    /// - The JWT does not have exactly three dot-separated parts.
    /// - The payload segment is not valid base64url (via
    ///   `base64_url_decode`).
    /// - The decoded payload is not valid JSON (via the [`From`] conversion
    ///   from [`serde_json::Error`]).
    /// - The payload object is missing a string-valued `audienceTenantId`
    ///   claim.
    pub fn tenant_id(&self) -> SalesforceAuthResult<String> {
        let parts: Vec<&str> = self.token.split('.').collect();
        if parts.len() != 3 {
            return Err(SalesforceAuthError::token_parse(
                "invalid DC JWT format: expected 3 parts",
            ));
        }

        let payload_b64 = parts[1];
        let payload_bytes = base64_url_decode(payload_b64)?;
        let payload: serde_json::Value = serde_json::from_slice(&payload_bytes)?;

        payload
            .get("audienceTenantId")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                SalesforceAuthError::token_parse("missing audienceTenantId in DC JWT payload")
            })
    }

    /// Returns the lakehouse name for Hyper connection.
    ///
    /// Format: `"lakehouse:<tenant_id>;<dataspace>"`
    ///
    /// # Errors
    ///
    /// Propagates any [`SalesforceAuthError::TokenParse`] from
    /// [`Self::tenant_id`] (malformed JWT structure, non-base64url payload,
    /// non-JSON payload, or missing `audienceTenantId` claim).
    pub fn lakehouse_name(&self, dataspace: Option<&str>) -> SalesforceAuthResult<String> {
        let tenant_id = self.tenant_id()?;
        let dataspace_str = dataspace.unwrap_or("");
        Ok(format!("lakehouse:{tenant_id};{dataspace_str}"))
    }
}

/// Decodes a base64url-encoded string.
fn base64_url_decode(input: &str) -> SalesforceAuthResult<Vec<u8>> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    // Handle both padded and unpadded base64url
    let padded = match input.len() % 4 {
        2 => format!("{input}=="),
        3 => format!("{input}="),
        _ => input.to_string(),
    };

    URL_SAFE_NO_PAD
        .decode(padded.trim_end_matches('='))
        .map_err(|e| SalesforceAuthError::token_parse(format!("base64 decode error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oauth_access_token_response_error() {
        let response = OAuthTokenResponse {
            access_token: String::new(),
            instance_url: String::new(),
            token_type: None,
            scope: None,
            issued_at: None,
            error: Some("invalid_grant".to_string()),
            error_description: Some("authentication failure".to_string()),
        };

        let result = response.check_error();
        assert!(result.is_err());
        if let Err(SalesforceAuthError::Authorization { error_code, .. }) = result {
            assert_eq!(error_code, "invalid_grant");
        } else {
            panic!("expected Authorization error");
        }
    }

    #[test]
    fn test_oauth_access_token_from_response() {
        let response = OAuthTokenResponse {
            access_token: "oauth_access_tok_123".to_string(),
            instance_url: "https://na1.salesforce.com".to_string(),
            token_type: Some("Bearer".to_string()),
            scope: None,
            issued_at: None,
            error: None,
            error_description: None,
        };

        let token = OAuthToken::from_response(response).unwrap();
        assert_eq!(token.token, "oauth_access_tok_123");
        assert_eq!(token.instance_url.as_str(), "https://na1.salesforce.com/");
        assert!(token.is_likely_valid());
        assert_eq!(token.bearer_token(), "Bearer oauth_access_tok_123");
    }

    #[test]
    fn test_dc_jwt_validity() {
        let response = DataCloudTokenResponse {
            access_token: "test.token.here".to_string(),
            instance_url: "https://tenant.salesforce.com".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600), // 1 hour
            error: None,
            error_description: None,
        };

        let token = DataCloudToken::from_response(response).unwrap();
        assert!(token.is_valid());
        assert!(!token.is_expired());
        assert_eq!(token.bearer_token(), "Bearer test.token.here");
        assert!(token.age().num_seconds() < 2);
        assert!(token.remaining_lifetime().num_seconds() > 3500);
    }

    #[test]
    fn test_dc_jwt_needs_refresh_when_fresh() {
        let response = DataCloudTokenResponse {
            access_token: "fresh.dc.jwt".to_string(),
            instance_url: "https://tenant.salesforce.com".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(7200),
            error: None,
            error_description: None,
        };

        let token = DataCloudToken::from_response(response).unwrap();
        // Fresh DC JWT (age ~0s): should NOT need refresh
        // threshold=300s (5min), maxAge=900s (15min)
        assert!(!token.needs_refresh(300, 900));
    }

    #[test]
    fn test_dc_jwt_needs_refresh_near_expiry() {
        let response = DataCloudTokenResponse {
            access_token: "expiring.dc.jwt".to_string(),
            instance_url: "https://tenant.salesforce.com".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(200), // expires in 200s (< 300s threshold)
            error: None,
            error_description: None,
        };

        let token = DataCloudToken::from_response(response).unwrap();
        // DC JWT with <300s remaining: SHOULD need refresh (expiring check)
        assert!(token.needs_refresh(300, 900));
    }

    #[test]
    fn test_dc_jwt_needs_refresh_too_old() {
        // Simulate an old DC JWT by backdating created_at
        let mut token = DataCloudToken::from_response(DataCloudTokenResponse {
            access_token: "old.dc.jwt".to_string(),
            instance_url: "https://tenant.salesforce.com".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(7200),
            error: None,
            error_description: None,
        })
        .unwrap();

        // Backdate created_at by 20 minutes (> 900s maxAge)
        token.created_at = Utc::now() - Duration::minutes(20);

        // DC JWT still has plenty of lifetime but is too old: SHOULD need refresh
        assert!(token.needs_refresh(300, 900));
    }

    #[test]
    fn test_dc_jwt_created_at_tracked() {
        let before = Utc::now();
        let response = DataCloudTokenResponse {
            access_token: "dc.jwt.value".to_string(),
            instance_url: "https://tenant.salesforce.com".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600),
            error: None,
            error_description: None,
        };
        let token = DataCloudToken::from_response(response).unwrap();
        let after = Utc::now();

        assert!(token.created_at() >= before);
        assert!(token.created_at() <= after);
    }

    #[test]
    fn test_dc_jwt_is_valid_uses_5min_buffer() {
        // A DC JWT with exactly 4 minutes remaining should NOT be considered valid
        // (below the 300s / 5-minute buffer)
        let response = DataCloudTokenResponse {
            access_token: "almost.expired.jwt".to_string(),
            instance_url: "https://tenant.salesforce.com".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(240), // 4 minutes
            error: None,
            error_description: None,
        };

        let token = DataCloudToken::from_response(response).unwrap();
        assert!(!token.is_valid());
        assert!(!token.is_expired()); // not yet hard-expired

        // A DC JWT with 6 minutes remaining SHOULD be valid
        let response2 = DataCloudTokenResponse {
            access_token: "still.valid.jwt".to_string(),
            instance_url: "https://tenant.salesforce.com".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(360), // 6 minutes
            error: None,
            error_description: None,
        };

        let token2 = DataCloudToken::from_response(response2).unwrap();
        assert!(token2.is_valid());
    }
}
