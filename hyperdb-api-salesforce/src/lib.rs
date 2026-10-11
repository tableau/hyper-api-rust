// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Salesforce Data Cloud authentication (OAuth Access Token + DC JWT).
//!
//! This crate implements the token flow for connecting to the Salesforce
//! Data Cloud Hyper query engine:
//!
//! 1. Obtain an **OAuth Access Token** from Salesforce
//! 2. Exchange it for a **DC JWT** (Data Cloud JSON Web Token)
//! 3. Send the DC JWT as the `Authorization` header with every gRPC call
//!
//! # Authentication Modes
//!
//! Three modes are supported for Step 1 (obtaining an OAuth Access Token):
//!
//! - **Password**: Username + password + client secret (OAuth password grant)
//! - **`PrivateKey`**: JWT Bearer Token Flow using RSA private key (recommended
//!   for server-to-server; no OAuth Refresh Token involved)
//! - **`RefreshToken`**: Uses a long-lived **OAuth Refresh Token** + client secret
//!
//! # Token Caching
//!
//! Both the OAuth Access Token and the DC JWT are cached independently.
//! The OAuth Access Token is only refreshed when genuinely expired, to avoid
//! unnecessary OAuth Refresh Token rotation that would invalidate tokens
//! held by other connections.  The DC JWT is refreshed when fewer than 5
//! minutes of its lifetime remain; [`DataCloudToken::needs_refresh`] adds an
//! age check for callers that want earlier refresh.
//!
//! # Example: JWT Bearer Token Flow
//!
//! ```no_run
//! use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, DataCloudTokenProvider};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let private_key_pem = std::fs::read_to_string("server.key")?;
//!
//! let config = SalesforceAuthConfig::new(
//!     "https://login.salesforce.com",
//!     "your-connected-app-client-id",
//! )?
//! .auth_mode(AuthMode::private_key("user@example.com", &private_key_pem)?);
//!
//! let mut provider = DataCloudTokenProvider::new(config)?;
//!
//! // Get a valid DC JWT (automatically handles OAuth Access Token + exchange)
//! let dc_jwt = provider.get_token().await?;
//!
//! println!("Authorization: {}", dc_jwt.bearer_token());
//! println!("Tenant URL: {}", dc_jwt.tenant_url_str());
//! # Ok(())
//! # }
//! ```
//!
//! # Two-Stage Token Flow
//!
//! 1. **OAuth Access Token**: Authenticate with Salesforce
//!    - Endpoint: `{login_url}/services/oauth2/token`
//!    - Returns: `access_token`, `instance_url`
//!
//! 2. **DC JWT**: Exchange OAuth Access Token for a Data Cloud JWT
//!    - Endpoint: `{instance_url}/services/a360/token`
//!    - Grant type: `urn:salesforce:grant-type:external:cdp`
//!    - Returns: `access_token` (the DC JWT), `instance_url`, `expires_in`
//!
//! # Security
//!
//! - Private keys are stored using `zeroize` for secure memory handling
//! - Tokens are cached and automatically refreshed before expiration
//! - All HTTP communication uses TLS

#![warn(missing_docs, rust_2018_idioms, clippy::all)]

mod config;
mod error;
mod jwt;
mod provider;
mod token;

pub use config::{AuthMode, SalesforceAuthConfig};
pub use error::{SalesforceAuthError, SalesforceAuthResult};
pub use provider::{DataCloudTokenProvider, SharedTokenProvider};
pub use token::{DataCloudToken, OAuthToken};
