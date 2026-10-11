# hyperdb-api-salesforce

Salesforce Data Cloud OAuth authentication for Hyper database.

This crate implements the two-stage OAuth token flow for connecting to Salesforce
Data Cloud via gRPC. It supports the JWT Bearer Token Flow, username-password
flow, and refresh token flow, with automatic token caching and refresh.

---

## Overview

`hyperdb-api-salesforce` is a companion crate that provides Salesforce-specific
authentication, separate from the core `hyperdb-api` and `hyperdb-api-core::client` crates.

- **Three OAuth flows** -- JWT Bearer Token (recommended), username-password, refresh token
- **Automatic token refresh** -- Proactive refresh before expiration, reactive on auth errors
- **Thread-safe** -- `SharedTokenProvider` for concurrent access via `Arc<Mutex<>>`
- **Secure memory** -- Private keys and secrets stored with `zeroize`
- **gRPC integration** -- Works with `AuthenticatedGrpcClient` from `hyperdb-api-core::client`

---

## Quick Start (JWT Bearer Token)

```rust
use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, DataCloudTokenProvider};

let private_key_pem = std::fs::read_to_string("private.key")?;

let config = SalesforceAuthConfig::new(
    "https://login.salesforce.com",
    "your-connected-app-consumer-key",
)?
.auth_mode(AuthMode::private_key("user@example.com", &private_key_pem)?);

let mut provider = DataCloudTokenProvider::new(config)?;
let token = provider.get_token().await?;

println!("Bearer token: {}", token.bearer_token());
println!("Tenant URL: {}", token.tenant_url_str());
```

---

## Authentication Flows

### JWT Bearer Token (Recommended)

Best for server-to-server authentication. Does not require a client secret.

```rust
let config = SalesforceAuthConfig::new(login_url, client_id)?
    .auth_mode(AuthMode::private_key("user@example.com", &private_key_pem)?);
```

| Parameter | Description |
|-----------|-------------|
| `login_url` | Salesforce login URL (e.g., `https://login.salesforce.com`) |
| `client_id` | External Client App Consumer Key |
| `username` | Salesforce username that authorized the app |
| `private_key_pem` | RSA private key in PKCS#8 PEM format |

### Username-Password

Requires `client_secret`. The password may need a security token suffix.

```rust
let config = SalesforceAuthConfig::new(login_url, client_id)?
    .client_secret("your-consumer-secret")
    .auth_mode(AuthMode::password("user@example.com", "password+securitytoken"));
```

### Refresh Token

Uses a long-lived OAuth refresh token. Requires `client_secret`.

```rust
let config = SalesforceAuthConfig::new(login_url, client_id)?
    .client_secret("your-consumer-secret")
    .auth_mode(AuthMode::refresh_token("your-refresh-token"));
```

---

## Token Management

`DataCloudTokenProvider` caches both the OAuth Access Token and the DC JWT
independently. The OAuth Access Token (~2-hour lifetime) is only refreshed when
genuinely expired, avoiding unnecessary refresh token rotation. The DC JWT is
refreshed when it is within 5 minutes of expiry. `get_token()` returns a
cached token when valid, or transparently performs the full flow when needed.
`AuthenticatedGrpcClient` additionally refreshes it once it is older than 15 minutes.

### SharedTokenProvider

For concurrent applications, wrap the provider in `SharedTokenProvider`:

```rust
use hyperdb_api_salesforce::SharedTokenProvider;

let provider = SharedTokenProvider::new(config)?;

// Clone and share across tasks
let provider_clone = provider.clone();
tokio::spawn(async move {
    let token = provider_clone.get_token().await?;
    // use token.bearer_token() as the Authorization header
    Ok::<(), hyperdb_api_salesforce::SalesforceAuthError>(())
});
```

`SharedTokenProvider` uses `Arc<Mutex<DataCloudTokenProvider>>` internally.
All token operations are serialized to prevent concurrent refresh races.

---

## Usage with AuthenticatedGrpcClient

Enable the `salesforce-auth` feature on `hyperdb-api-core::client`:

```toml
[dependencies]
hyperdb-api-core = { version = "1.0", features = ["salesforce-auth"] }
hyperdb-api-salesforce = "1.0"
```

`AuthenticatedGrpcClient` handles token refresh automatically, including
reconnection with new tokens:

```rust
use hyperdb_api_salesforce::{SalesforceAuthConfig, AuthMode, SharedTokenProvider};
use hyperdb_api_core::client::grpc::AuthenticatedGrpcClient;

let private_key_pem = std::fs::read_to_string("private.key")?;
let auth_config = SalesforceAuthConfig::new(login_url, client_id)?
    .auth_mode(AuthMode::private_key("user@example.com", &private_key_pem)?);

let token_provider = SharedTokenProvider::new(auth_config)?;
let mut client = AuthenticatedGrpcClient::connect(token_provider, None).await?;

// Token refresh is handled automatically, even for long-running queries
let result = client.execute_query("SELECT * FROM my_table").await?;

// Built-in catalog operations
let tables = client.list_tables().await?;
```

**Refresh strategy:**

1. **Proactive** -- Refreshes the DC JWT before expiration (5-minute buffer) and
   when it exceeds a maximum age (15 minutes by default)
2. **Reactive** -- On gRPC auth errors (UNAUTHENTICATED), refreshes and retries once

---

## Setup Guide

### Step 1: Generate RSA Key Pair

```bash
# Generate a 2048-bit RSA private key
openssl genrsa -out keypair.key 2048

# Create a self-signed certificate (valid 1 year)
openssl req -new -x509 -nodes -sha256 -days 365 -key keypair.key -out certificate.crt

# Convert to PKCS#8 format (required by this crate)
openssl pkcs8 -topk8 -nocrypt -in keypair.key -out private.key
```

- `certificate.crt` -- Upload to Salesforce External Client App
- `private.key` -- Use in your application (keep secure)

### Step 2: Create External Client App in Salesforce

1. **Setup > External Client App Manager > New External Client App**
2. Flow Type: **App acts on behalf of a user**
3. Add OAuth scopes:
   - Access the identity URL service (`id`, `profile`, `email`, `address`, `phone`)
   - Manage user data via APIs (`api`)
   - Perform requests at any time (`refresh_token`, `offline_access`)
   - **Perform ANSI SQL queries on Data Cloud data (`cdp_query_api`)** -- required
4. Callback URL: `https://localhost:1717/OauthRedirect`
5. Enable JWT Bearer Flow and upload `certificate.crt`

### Step 3: Pre-authorize Users

1. External Client App Manager > Your app > **Policies** tab
2. Set Permitted Users to **Admin approved users are pre-authorized**
3. **Manage Profiles** > Add profiles that should have access

### Step 4: Get Consumer Key

1. External Client App Manager > Your app > **OAuth Settings**
2. Click **Manage Consumer Details** (verify identity)
3. Copy the **Consumer Key** (this is the `client_id`)

---

## Environment Variables

The included example reads configuration from environment variables:

```bash
export SF_LOGIN_URL="https://login.salesforce.com"
export SF_CLIENT_ID="3MVG9..."
export SF_USERNAME="your-email@company.com"
export SF_PRIVATE_KEY_PATH="/path/to/private.key"
export SF_DATASPACE="default"  # optional
```

```bash
cargo run -p hyperdb-api-salesforce --example salesforce_auth_example
```

---

## Configuration Options

| Builder method | Default | Description |
|----------------|---------|-------------|
| `.client_secret(s)` | `None` | Required for Password and RefreshToken modes |
| `.dataspace(s)` | `None` | Data Cloud dataspace name |
| `.timeout_secs(n)` | `30` | HTTP request timeout |
| `.max_retries(n)` | `3` | Max retries for transient (5xx) failures |

**Login URLs:** `https://login.salesforce.com` (production),
`https://test.salesforce.com` (sandbox), or
`https://mydomain.my.salesforce.com` (custom domain).

---

## Troubleshooting

| Error | Cause and Solution |
|-------|-------------------|
| `authorization failed: invalid_grant - user hasn't approved this consumer` | Pre-authorize the user (Step 3 above) |
| `authorization failed: invalid_client_id - ...` | Verify Consumer Key matches exactly |
| `private key error: failed to parse private key (expected PKCS#8 PEM format): ...` | Convert to PKCS#8: `openssl pkcs8 -topk8 -nocrypt -in keypair.key -out private.key` |
| `authorization failed: invalid_grant - authentication failure` | Check username, login URL, and certificate match |
| `authorization failed: ...` or `HTTP error: ...` after a `DC JWT exchange failed` warning in the logs | Verify Data Cloud license and `cdp_query_api` scope |
| `configuration error: client_secret is required for Password and RefreshToken auth modes` | Add `.client_secret(...)` to your config |

---

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tableau/hyper-api-rust/blob/main/LICENSE-APACHE.txt) or [MIT license](https://github.com/tableau/hyper-api-rust/blob/main/LICENSE-MIT.txt), at your option.
