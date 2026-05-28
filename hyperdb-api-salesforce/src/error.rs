// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Error types for Salesforce Data Cloud authentication.

use std::fmt;

/// Result type for Salesforce authentication operations.
pub type SalesforceAuthResult<T> = Result<T, SalesforceAuthError>;

/// Errors that can occur during the Salesforce Data Cloud token flow
/// (OAuth Access Token acquisition and DC JWT exchange).
#[derive(Debug)]
pub enum SalesforceAuthError {
    /// Invalid configuration (missing required fields, invalid URLs, etc.)
    Config(String),

    /// Failed to parse or load the RSA private key for JWT Bearer Token Flow
    PrivateKey(String),

    /// JWT assertion creation or signing failed
    Jwt(String),

    /// HTTP request failed (network-level error)
    Http(String),

    /// OAuth Access Token or DC JWT request rejected by Salesforce (4xx response)
    Authorization {
        /// Error code from Salesforce (e.g., "`invalid_grant`")
        error_code: String,
        /// Human-readable error description
        error_description: String,
    },

    /// DC JWT exchange failed
    TokenExchange(String),

    /// Token response parsing failed (invalid response format)
    TokenParse(String),

    /// DC JWT has expired
    TokenExpired,

    /// Network or I/O error
    Io(String),
}

impl SalesforceAuthError {
    /// Constructs a [`Self::Config`] error.
    pub fn config(message: impl Into<String>) -> Self {
        SalesforceAuthError::Config(message.into())
    }

    /// Constructs a [`Self::PrivateKey`] error.
    pub fn private_key(message: impl Into<String>) -> Self {
        SalesforceAuthError::PrivateKey(message.into())
    }

    /// Constructs a [`Self::Jwt`] error.
    pub fn jwt(message: impl Into<String>) -> Self {
        SalesforceAuthError::Jwt(message.into())
    }

    /// Constructs a [`Self::Http`] error.
    pub fn http(message: impl Into<String>) -> Self {
        SalesforceAuthError::Http(message.into())
    }

    /// Constructs a [`Self::Authorization`] error.
    pub fn authorization(
        error_code: impl Into<String>,
        error_description: impl Into<String>,
    ) -> Self {
        SalesforceAuthError::Authorization {
            error_code: error_code.into(),
            error_description: error_description.into(),
        }
    }

    /// Constructs a [`Self::TokenExchange`] error.
    pub fn token_exchange(message: impl Into<String>) -> Self {
        SalesforceAuthError::TokenExchange(message.into())
    }

    /// Constructs a [`Self::TokenParse`] error.
    pub fn token_parse(message: impl Into<String>) -> Self {
        SalesforceAuthError::TokenParse(message.into())
    }

    /// Constructs a [`Self::Io`] error.
    pub fn io(message: impl Into<String>) -> Self {
        SalesforceAuthError::Io(message.into())
    }
}

impl fmt::Display for SalesforceAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SalesforceAuthError::Config(msg) => write!(f, "configuration error: {msg}"),
            SalesforceAuthError::PrivateKey(msg) => write!(f, "private key error: {msg}"),
            SalesforceAuthError::Jwt(msg) => write!(f, "JWT assertion error: {msg}"),
            SalesforceAuthError::Http(msg) => write!(f, "HTTP error: {msg}"),
            SalesforceAuthError::Authorization {
                error_code,
                error_description,
            } => write!(
                f,
                "authorization failed: {error_code} - {error_description}"
            ),
            SalesforceAuthError::TokenExchange(msg) => {
                write!(f, "DC JWT exchange failed: {msg}")
            }
            SalesforceAuthError::TokenParse(msg) => write!(f, "token parse error: {msg}"),
            SalesforceAuthError::TokenExpired => write!(f, "DC JWT has expired"),
            SalesforceAuthError::Io(msg) => write!(f, "I/O error: {msg}"),
        }
    }
}

impl std::error::Error for SalesforceAuthError {}

impl From<reqwest::Error> for SalesforceAuthError {
    fn from(err: reqwest::Error) -> Self {
        SalesforceAuthError::Http(err.to_string())
    }
}

impl From<jsonwebtoken::errors::Error> for SalesforceAuthError {
    fn from(err: jsonwebtoken::errors::Error) -> Self {
        SalesforceAuthError::Jwt(err.to_string())
    }
}

impl From<rsa::pkcs8::Error> for SalesforceAuthError {
    fn from(err: rsa::pkcs8::Error) -> Self {
        SalesforceAuthError::PrivateKey(err.to_string())
    }
}

impl From<url::ParseError> for SalesforceAuthError {
    fn from(err: url::ParseError) -> Self {
        SalesforceAuthError::Config(format!("invalid URL: {err}"))
    }
}

impl From<serde_json::Error> for SalesforceAuthError {
    fn from(err: serde_json::Error) -> Self {
        SalesforceAuthError::TokenParse(err.to_string())
    }
}
