// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Authentication mechanisms for Hyper connections.
//!
//! The server selects the authentication method during the startup handshake by
//! sending an `AuthenticationRequest` message. This module provides the
//! client-side implementations for each supported method:
//!
//! | Method | Server message | Client response |
//! |--------|---------------|-----------------|
//! | Trust | `AuthenticationOk` | (none) |
//! | Cleartext | `AuthenticationCleartextPassword` | Password in plain text |
//! | MD5 | `AuthenticationMD5Password(salt)` | `"md5" + MD5(MD5(password+user) + salt)` |
//! | SCRAM-SHA-256 | `AuthenticationSASL` | Multi-step: client-first, server-first, client-final, server-final |
//!
//! # SCRAM-SHA-256 Protocol (RFC 5802)
//!
//! The SCRAM exchange is a 4-message handshake managed by [`AuthState`]:
//!
//! 1. **Client-first** ([`scram_client_first`]) — Client sends `n,,n=,r=<nonce>`
//! 2. **Server-first** — Server responds with `r=<combined-nonce>,s=<salt>,i=<iterations>`
//! 3. **Client-final** ([`scram_client_final`]) — Client derives keys via PBKDF2-SHA-256
//!    and sends proof: `c=<channel-binding>,r=<nonce>,p=<client-proof>`
//! 4. **Server-final** ([`scram_verify_server`]) — Client verifies server signature
//!
//! # Security
//!
//! This module uses `zeroize` to securely clear sensitive cryptographic material
//! (passwords, derived keys, HMAC outputs) from memory when they go out of
//! scope. All intermediate key material in the SCRAM exchange is wrapped in
//! [`Zeroizing<Vec<u8>>`](zeroize::Zeroizing) to prevent memory disclosure.
//!
//! # Attribution
//!
//! Portions of this module's SCRAM-SHA-256 implementation were adapted from
//! [`postgres-protocol`](https://github.com/sfackler/rust-postgres)'s
//! `authentication/sasl.rs` (Copyright (c) 2016 Steven Fackler, MIT or
//! Apache-2.0). Adapted material includes the variable naming
//! (`client_first_bare`, `salted_password`, `client_key`, `server_key`,
//! `stored_key`, `client_signature`, `client_proof`, `auth_message`) and the
//! key-derivation sequence. The field-parsing structure was rewritten;
//! Hyper-specific changes added include `zeroize`-based memory hygiene of
//! derived key material. See the `NOTICE` file at the repo root for the full
//! upstream copyright and reproduced license text.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use hmac::{Hmac, KeyInit, Mac};
use md5::{Digest, Md5};
use pbkdf2::pbkdf2_hmac;
use rand::RngExt;
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

use super::error::{Error, Result};

/// Computes the MD5 password hash for `PostgreSQL` authentication.
///
/// The format is: "md5" + MD5(MD5(password + user) + salt)
#[must_use]
pub fn compute_md5_password(user: &str, password: &str, salt: &[u8]) -> String {
    // First hash: MD5(password + user)
    let mut hasher = Md5::new();
    hasher.update(password.as_bytes());
    hasher.update(user.as_bytes());
    let first_hash = hasher.finalize();

    // Convert to hex string
    let first_hex = hex_encode(&first_hash);

    // Second hash: MD5(first_hex + salt)
    let mut hasher = Md5::new();
    hasher.update(first_hex.as_bytes());
    hasher.update(salt);
    let second_hash = hasher.finalize();

    // Format: "md5" + hex(second_hash)
    format!("md5{}", hex_encode(&second_hash))
}

#[expect(
    clippy::format_collect,
    reason = "readable hex/string formatting loop; refactoring to fold! obscures intent"
)]
/// Converts bytes to lowercase hex string.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// State for SCRAM-SHA-256 authentication exchange.
///
/// Sensitive cryptographic material (password, derived keys) is automatically
/// zeroized when this struct is dropped to prevent memory disclosure attacks.
///
/// This struct maintains the state needed for the multi-step SCRAM-SHA-256
/// authentication protocol:
/// 1. Client sends client-first message
/// 2. Server responds with server-first message (salt, iterations, nonce)
/// 3. Client computes keys and sends client-final message
/// 4. Server responds with server-final message (signature verification)
#[derive(Debug)]
pub struct AuthState {
    /// Password - automatically zeroized when dropped.
    password: Zeroizing<String>,
    /// Client-generated random nonce for this authentication exchange.
    client_nonce: String,
    /// Client-first message without GS2 header (for auth message construction).
    client_first_bare: String,
    /// Server-first message (stored for auth message construction).
    #[allow(
        dead_code,
        reason = "retained for future re-authentication flows that replay the SCRAM exchange"
    )]
    server_first: Option<String>,
    /// Complete authentication message (client-first + server-first + client-final).
    auth_message: Option<String>,
    /// Server key (derived from password) - automatically zeroized when dropped.
    server_key: Option<Zeroizing<Vec<u8>>>,
}

/// Generates the client-first message for SCRAM-SHA-256.
///
/// Returns the auth state and the client-first message to send.
/// The password is stored securely and will be zeroized when the `AuthState` is dropped.
///
/// # Errors
///
/// Currently infallible — always returns `Ok`. The `Result` return type
/// is preserved for forward compatibility so future validation (password
/// length, encoding checks) can surface errors without a signature
/// change.
pub fn scram_client_first(password: &str) -> Result<(AuthState, Vec<u8>)> {
    // Generate a random nonce
    let client_nonce = generate_nonce();

    // Build the client-first-message-bare (without GS2 header)
    // Format: n=<username>,r=<nonce>
    // We use empty username since Hyper uses separate user parameter
    let client_first_bare = format!("n=,r={client_nonce}");

    // Build the full client-first-message
    // Format: <gs2-header><client-first-message-bare>
    // GS2 header: n,, (no channel binding, no authzid)
    let client_first = format!("n,,{client_first_bare}");

    let state = AuthState {
        password: Zeroizing::new(password.to_string()),
        client_nonce,
        client_first_bare,
        server_first: None,
        auth_message: None,
        server_key: None,
    };

    Ok((state, client_first.into_bytes()))
}

/// Processes the server-first message and generates the client-final message.
///
/// Returns the updated auth state and the client-final message to send.
///
/// # Errors
///
/// Returns [`Error`] (auth) when:
/// - `server_first` is not valid UTF-8.
/// - The iteration count (`i=`), nonce (`r=`), or salt (`s=`) field is
///   missing or cannot be parsed.
/// - The server nonce does not start with the client-generated nonce
///   (an anti-MITM guarantee required by the SCRAM spec).
/// - The salt cannot be base64-decoded.
pub fn scram_client_final(
    mut state: AuthState,
    server_first: &[u8],
) -> Result<(AuthState, Vec<u8>)> {
    let server_first_str = std::str::from_utf8(server_first)
        .map_err(|_| Error::authentication("invalid UTF-8 in server-first message"))?;

    // Parse server-first message
    // Format: r=<nonce>,s=<salt>,i=<iterations>
    let mut server_nonce = None;
    let mut salt_b64 = None;
    let mut iterations = None;

    for part in server_first_str.split(',') {
        if let Some(value) = part.strip_prefix("r=") {
            server_nonce = Some(value);
        } else if let Some(value) = part.strip_prefix("s=") {
            salt_b64 = Some(value);
        } else if let Some(value) = part.strip_prefix("i=") {
            iterations = Some(value.parse::<u32>().map_err(|_| {
                Error::authentication("invalid iteration count in server-first message")
            })?);
        }
    }

    let server_nonce = server_nonce
        .ok_or_else(|| Error::authentication("missing nonce in server-first message"))?;
    let salt_b64 =
        salt_b64.ok_or_else(|| Error::authentication("missing salt in server-first message"))?;
    let iterations = iterations
        .ok_or_else(|| Error::authentication("missing iterations in server-first message"))?;

    // Verify server nonce starts with client nonce.
    // Use a constant-time comparison to avoid leaking the client nonce prefix
    // length via timing observation by a network adversary.
    let client_nonce_bytes = state.client_nonce.as_bytes();
    let server_nonce_bytes = server_nonce.as_bytes();
    let prefix_match = server_nonce_bytes.len() >= client_nonce_bytes.len() && {
        let mut diff: u8 = 0;
        for (a, b) in server_nonce_bytes
            .iter()
            .zip(client_nonce_bytes.iter())
            .take(client_nonce_bytes.len())
        {
            diff |= a ^ b;
        }
        diff == 0
    };
    if !prefix_match {
        return Err(Error::authentication(
            "server nonce doesn't match client nonce",
        ));
    }

    // Decode salt
    let salt = BASE64
        .decode(salt_b64)
        .map_err(|_| Error::authentication("invalid base64 in salt"))?;

    // Derive keys using PBKDF2
    // Use Zeroizing to ensure sensitive key material is cleared from memory
    let salted_password: Zeroizing<Vec<u8>> =
        Zeroizing::new(pbkdf2_sha256(&state.password, &salt, iterations));

    let client_key: Zeroizing<Vec<u8>> =
        Zeroizing::new(hmac_sha256(&salted_password, b"Client Key"));
    let server_key: Zeroizing<Vec<u8>> =
        Zeroizing::new(hmac_sha256(&salted_password, b"Server Key"));
    // stored_key is SHA256(client_key) - a derived cryptographic key that should be zeroized
    let stored_key: Zeroizing<Vec<u8>> = Zeroizing::new(sha256(&client_key));

    // Build client-final-message-without-proof
    // Format: c=<channel-binding>,r=<nonce>
    // Channel binding is base64("n,,") since we used n,, in client-first
    let channel_binding_b64 = BASE64.encode(b"n,,");
    let client_final_without_proof = format!("c={channel_binding_b64},r={server_nonce}");

    // Build auth message
    let auth_message = format!(
        "{},{},{}",
        state.client_first_bare, server_first_str, client_final_without_proof
    );

    // Compute client signature and proof
    // client_signature is a cryptographic signature derived from stored_key - should be zeroized
    let client_signature: Zeroizing<Vec<u8>> =
        Zeroizing::new(hmac_sha256(&stored_key, auth_message.as_bytes()));
    let mut client_proof: Zeroizing<Vec<u8>> = Zeroizing::new(
        client_key
            .iter()
            .zip(client_signature.iter())
            .map(|(k, s)| k ^ s)
            .collect(),
    );

    // Build client-final message
    let client_final = format!(
        "{},p={}",
        client_final_without_proof,
        BASE64.encode(client_proof.as_slice())
    );

    // Explicitly zeroize client_proof now that we've encoded it
    client_proof.zeroize();

    // Update state
    state.server_first = Some(server_first_str.to_string());
    state.auth_message = Some(auth_message);
    state.server_key = Some(server_key);

    Ok((state, client_final.into_bytes()))
}

/// Verifies the server-final message.
///
/// This function consumes the `AuthState`, ensuring all sensitive cryptographic
/// material is zeroized after verification completes.
///
/// # Errors
///
/// Returns [`Error`] (auth) when:
/// - `server_final` is not valid UTF-8.
/// - The payload is missing the `v=` server-signature prefix.
/// - The server signature cannot be base64-decoded.
/// - The SCRAM state is incomplete (missing `AuthState::server_key`
///   or `AuthState::auth_message`), indicating the caller did not
///   run [`scram_client_final`] first.
/// - The computed server signature does not match the one provided by
///   the server (indicates the server does not know the user's
///   password).
pub fn scram_verify_server(state: AuthState, server_final: &[u8]) -> Result<()> {
    let server_final_str = std::str::from_utf8(server_final)
        .map_err(|_| Error::authentication("invalid UTF-8 in server-final message"))?;

    // Parse server signature
    // Format: v=<server-signature>
    let server_sig_b64 = server_final_str
        .strip_prefix("v=")
        .ok_or_else(|| Error::authentication("invalid server-final message format"))?;

    // Decoded server signature - wrap in Zeroizing for secure memory handling
    let server_sig: Zeroizing<Vec<u8>> = Zeroizing::new(
        BASE64
            .decode(server_sig_b64)
            .map_err(|_| Error::authentication("invalid base64 in server signature"))?,
    );

    // Compute expected server signature
    let server_key = state
        .server_key
        .ok_or_else(|| Error::authentication("missing server key in auth state"))?;
    let auth_message = state
        .auth_message
        .ok_or_else(|| Error::authentication("missing auth message in auth state"))?;

    // expected_sig is a cryptographic signature - should be zeroized
    let expected_sig: Zeroizing<Vec<u8>> =
        Zeroizing::new(hmac_sha256(&server_key, auth_message.as_bytes()));

    // Verify signatures match (all sensitive material is zeroized when dropped)
    if server_sig.as_slice() != expected_sig.as_slice() {
        return Err(Error::authentication(
            "server signature verification failed",
        ));
    }

    // AuthState is dropped here, zeroizing all sensitive material
    Ok(())
}

/// Generates a random base64-encoded nonce.
fn generate_nonce() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 18] = rng.random();
    BASE64.encode(bytes)
}

/// PBKDF2 with SHA-256 for SCRAM.
fn pbkdf2_sha256(password: &str, salt: &[u8], iterations: u32) -> Vec<u8> {
    let mut result = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut result);
    result.to_vec()
}

/// HMAC-SHA256.
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    type HmacSha256 = Hmac<Sha256>;

    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// SHA-256 hash.
fn sha256(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_md5_password() {
        // Test vector from PostgreSQL documentation
        let result = compute_md5_password("user", "password", &[0x01, 0x02, 0x03, 0x04]);
        assert!(result.starts_with("md5"));
        assert_eq!(result.len(), 35); // "md5" + 32 hex chars
    }

    #[test]
    fn test_hex_encode() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x12, 0xab]), "00ff12ab");
    }

    #[test]
    fn test_generate_nonce() {
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();
        assert_ne!(nonce1, nonce2);
        assert!(!nonce1.is_empty());
    }
}
