// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! JWT assertion generation for the Salesforce JWT Bearer Token Flow.
//!
//! Used by [`AuthMode::PrivateKey`](super::config::AuthMode::PrivateKey) to
//! obtain an OAuth Access Token without an OAuth Refresh Token.
//!
//! See: <https://help.salesforce.com/s/articleView?id=xcloud.remoteaccess_oauth_jwt_flow.htm>

use chrono::{Duration, Utc};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use rsa::RsaPrivateKey;
use serde::Serialize;

use crate::error::{SalesforceAuthError, SalesforceAuthResult};

/// JWT claims for the Salesforce JWT Bearer Token Flow assertion.
///
/// Standard JWT claims as specified in RFC 7519, with Salesforce-specific
/// requirements.  This assertion is `POSTed` to `/services/oauth2/token` to
/// obtain an OAuth Access Token.
#[derive(Debug, Serialize)]
struct JwtClaims {
    /// Issuer — Connected App Consumer Key (`client_id`)
    iss: String,

    /// Subject — Salesforce username
    sub: String,

    /// Audience — Salesforce authorization server URL
    aud: String,

    /// Issued at time (Unix timestamp)
    iat: i64,

    /// Expiration time (Unix timestamp)
    exp: i64,
}

/// Builds a JWT assertion for the Salesforce JWT Bearer Token Flow.
///
/// The assertion is signed with RS256 and `POSTed` to
/// `/services/oauth2/token` to obtain an OAuth Access Token (without
/// requiring an OAuth Refresh Token).
///
/// # Arguments
///
/// * `client_id` - Connected App Consumer Key
/// * `username` - Salesforce username (email)
/// * `login_url` - Salesforce login URL (scheme + host only)
/// * `private_key` - RSA private key for signing
///
/// # Returns
///
/// A compact JWT string (base64url-encoded header.payload.signature)
pub(crate) fn build_jwt_assertion(
    client_id: &str,
    username: &str,
    login_url: &url::Url,
    private_key: &RsaPrivateKey,
) -> SalesforceAuthResult<String> {
    let now = Utc::now();

    // Build audience URL (scheme + host only, no path)
    let audience = format!(
        "{}://{}",
        login_url.scheme(),
        login_url.host_str().unwrap_or("login.salesforce.com")
    );

    // JWT must expire within 5 minutes per Salesforce spec, we use 2 minutes for safety
    let claims = JwtClaims {
        iss: client_id.to_string(),
        sub: username.to_string(),
        aud: audience,
        iat: now.timestamp(),
        exp: (now + Duration::minutes(2)).timestamp(),
    };

    // Convert RSA private key to PEM for jsonwebtoken
    let private_key_pem = private_key_to_pem(private_key)?;
    let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
        .map_err(|e| SalesforceAuthError::jwt(format!("failed to create encoding key: {e}")))?;

    // Create JWT header with RS256 algorithm
    let header = Header::new(Algorithm::RS256);

    // Encode the JWT
    let token = encode(&header, &claims, &encoding_key)?;

    tracing::debug!(
        iss = %client_id,
        sub = %username,
        aud = %claims.aud,
        "JWT assertion created"
    );

    Ok(token)
}

/// Converts an RSA private key to PEM format.
fn private_key_to_pem(key: &RsaPrivateKey) -> SalesforceAuthResult<String> {
    use rsa::pkcs8::EncodePrivateKey;

    key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .map(|pem| pem.to_string())
        .map_err(|e| SalesforceAuthError::private_key(format!("failed to encode private key: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate an RSA private key at runtime so no PEM literals appear in source.
    ///
    /// Uses `rsa`'s re-exported `rand_core::OsRng` (which is `rand 0.8`-compatible)
    /// rather than `rand::rng()` from workspace `rand 0.10`, because `rsa 0.9`'s
    /// `RsaPrivateKey::new` bound requires `rand_core 0.6`'s `CryptoRngCore`. The
    /// two `rand_core` versions are incompatible at the trait level; this can be
    /// simplified once `rsa 0.10` stabilizes (currently at `rc.18`).
    fn generate_test_key() -> RsaPrivateKey {
        let mut rng = rsa::rand_core::OsRng;
        RsaPrivateKey::new(&mut rng, 2048).expect("failed to generate RSA key")
    }

    #[test]
    fn test_build_jwt_assertion() {
        let private_key = generate_test_key();
        let login_url = url::Url::parse("https://login.salesforce.com").unwrap();

        let jwt = build_jwt_assertion(
            "test-client-id",
            "user@example.com",
            &login_url,
            &private_key,
        )
        .unwrap();

        // JWT should have 3 parts separated by dots
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);

        // Verify we can decode the header
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let header_json = URL_SAFE_NO_PAD.decode(parts[0]).unwrap();
        let header: serde_json::Value = serde_json::from_slice(&header_json).unwrap();
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
    }
}
