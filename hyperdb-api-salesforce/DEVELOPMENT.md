# hyperdb-api-salesforce Development Guide

Internal architecture and contributor notes for the `hyperdb-api-salesforce` crate.

---

## Two-Stage OAuth Flow

Authentication with Salesforce Data Cloud uses a two-stage token exchange:

```text
Stage 1: App --> POST {login_url}/services/oauth2/token --> OAuth Access Token
Stage 2: App --> POST {instance_url}/services/a360/token --> DC JWT
```

**Stage 1** obtains an OAuth Access Token from Salesforce. The grant type
depends on the configured `AuthMode`:

| AuthMode | grant_type | Key fields |
|----------|------------|------------|
| PrivateKey | `urn:ietf:params:oauth:grant-type:jwt-bearer` | `assertion` (signed JWT) |
| Password | `password` | `username`, `password`, `client_secret` |
| RefreshToken | `refresh_token` | `refresh_token`, `client_secret` |

**Stage 2** exchanges the OAuth Access Token for a DC JWT (Data Cloud JSON Web
Token) using grant type `urn:salesforce:grant-type:external:cdp`. The DC JWT is
sent as the `Authorization: Bearer` header with every gRPC call.

If Stage 2 fails, the provider retries once with a force-refreshed OAuth Access
Token (Step 2a). This handles the case where the OAuth Access Token appeared
valid locally but was invalidated server-side by Salesforce's inactivity timeout.

See `provider.rs` (`fetch_dc_jwt`) for the implementation.

---

## Token Caching Strategy

The OAuth Access Token and DC JWT are cached independently in
`DataCloudTokenProvider`:

- **OAuth Access Token** (~2-hour reported lifetime): Only refreshed when
  `is_likely_valid()` returns false. This avoids unnecessary refresh token
  rotation that would invalidate tokens held by other connections sharing the
  same refresh token.

- **DC JWT** (~2-hour hard expiry): the provider refreshes it when `is_valid()`
  fails, meaning fewer than `DC_JWT_VALIDITY_BUFFER_SECS` (300 s) remain. The
  stricter `needs_refresh(threshold, max_age)` check (expiry threshold *or* age
  over 900 s) is applied by `AuthenticatedGrpcClient` in `hyperdb-api-core`,
  which owns those constants (`DEFAULT_DC_JWT_EXPIRY_THRESHOLD_SECS`,
  `DEFAULT_DC_JWT_MAX_AGE_SECS`).

---

## Testing

Unit tests cover token parsing, validity checks, and JWT assertion generation.
`tests/crypto_provider_tests.rs` guards the `rustls-no-provider` choice by
constructing a provider (no network or credentials needed):

```bash
cargo test -p hyperdb-api-salesforce
```

Integration tests against a live Salesforce org require environment variables
(see README). The example serves as the primary integration test:

```bash
cargo run -p hyperdb-api-salesforce --example salesforce_auth_example
```

There are no mock HTTP tests currently; the `post_with_retry` method makes
direct HTTP calls. Future work could introduce a trait-based HTTP abstraction
for testing.
