# Migrating to v0.3.0

This is the consolidated migration guide for the v0.3.0 bundle of breaking and additive changes. Each section corresponds to a bundle PR; the guide grows as each PR lands. The bundle ships as one major bump after the last PR merges.

> Each bundle PR uses `chore:` Conventional Commit prefix to defer release-please from cutting an early version. After all PRs merge, a single `feat!:` commit with a `BREAKING CHANGE:` footer triggers v0.3.0.

---

## #70 — Flatten the public `Error` enum

The public `hyperdb_api::Error` type was redesigned into a flat enum per the [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/) (M-ERRORS-CANONICAL-STRUCTS, M-ERRORS-AVOID-WRAPPING-AND-AS-DYN). Callers now match directly on variants instead of going through `kind() -> Option<ErrorKind>`.

### What's gone

| Removed                              | Status                                       |
| ------------------------------------ | -------------------------------------------- |
| `Error::Client(client::Error)`       | Variant deleted; `client::Error` is mapped to flat variants via internal `From` impl. |
| `Error::Other { message, source }`   | Variant deleted; the `Box<dyn StdError>` cause channel is gone. |
| `Error::new(msg)`                    | Constructor deleted. Use a specific variant or `Error::internal(msg)` (see below). |
| `Error::with_cause(msg, e)`          | Constructor deleted. For an `io::Error` cause use `Error::connection_with_io(msg, e)`; otherwise fold the cause into a message string. |
| `Error::kind() -> Option<ErrorKind>` | Method deleted. Match directly on the enum. |
| `pub use ... ErrorKind` from `hyperdb_api`              | Re-export removed. The `ErrorKind` type is internal to `hyperdb-api-core` and not part of `hyperdb-api`'s public surface. |

### What's new

```rust
pub enum Error {
    // Connection / transport
    Connection { message: String, source: Option<std::io::Error> },
    Authentication(String),
    Tls(String),

    // Server-side
    Server { sqlstate: Option<String>, message: String, detail: Option<String>, hint: Option<String> },
    Protocol(String),

    // I/O
    Io(std::io::Error),

    // Lifecycle
    Closed(String),
    Timeout(String),
    Cancelled(String),

    // Type / value
    Conversion(String),
    Config(String),
    FeatureNotSupported(String),

    // Catalog / validation
    InvalidName(String),
    InvalidTableDefinition(String),
    NotFound(String),
    AlreadyExists(String),

    // Column / row mapping
    Column { name: String, kind: ColumnErrorKind },
    ColumnIndexOutOfBounds { idx: usize, column_count: usize },

    // Internal
    Internal { message: String },
}

pub enum ColumnErrorKind {
    Missing,
    Null,
    TypeMismatch { expected: String, actual: String },
}
```

The enum is `#[non_exhaustive]`. Match arms must include a wildcard `_ =>` pattern.

### Constructors

Every variant has a snake_case constructor that takes `impl Into<String>` for any string field. Use these instead of struct-expression or tuple-construction syntax — they accept `&str`, `String`, `format!(...)`, and any other `Into<String>` source without `.to_string()` ceremony.

```rust
// Struct variants
Error::internal("invariant violated: ...");
Error::connection("failed to connect");
Error::connection_with_io("read failed", io_err);                       // io_err: std::io::Error
Error::server(sqlstate, message, detail, hint);                         // all four args
Error::column("user_id", ColumnErrorKind::Missing);
Error::column_index_out_of_bounds(idx, column_count);

// Tuple variants
Error::authentication("...");
Error::tls("...");
Error::protocol("...");
Error::closed("...");
Error::timeout("...");
Error::cancelled("...");
Error::conversion("...");
Error::config("...");
Error::feature_not_supported("...");
Error::invalid_name("...");
Error::invalid_table_definition("...");
Error::not_found("...");
Error::already_exists("...");
```

Pattern-matching uses the PascalCase variant names (e.g. `Error::Conversion(msg)`); only construction switches to snake_case. Forward-compatibility for new struct-variant fields relies on going through these constructors — `#[non_exhaustive]` on individual struct variants is forbidden by Rust E0639.

### Behavioral note: SQLSTATE on non-server errors

`Error::sqlstate()` now returns `Some(...)` only for [`Error::Server`]. Previously it could return `Some` for any wrapped `client::Error` whose internal type carried a SQLSTATE code, including some `Cancelled`, `Closed`, and `Connection` paths that arrived from the server with codes like `57014` (`query_canceled`), `57P03` (`cannot_connect_now`), or `08*` connection-class codes.

After v0.3.0 those SQLSTATE codes are folded into the variant's message string (still visible to humans via `Display`) but are not surfaced by `Error::sqlstate()`. If you branch on those codes, parse them out of the message string or open a follow-up issue requesting structured SQLSTATE on `Connection`/`Closed`/`Cancelled`/`Timeout` variants.

### Migration recipes

**Match on error kind** — before:
```rust
match err.kind() {
    Some(ErrorKind::Connection) => retry(),
    Some(ErrorKind::Authentication) => prompt_creds(),
    _ => return Err(err),
}
```

after:
```rust
match err {
    Error::Connection { .. } => retry(),
    Error::Authentication(_) => prompt_creds(),
    other => return Err(other),
}
```

**Wrap an `io::Error`** — before:
```rust
return Err(Error::with_cause("read failed", io_err));
```

after:
```rust
return Err(Error::connection_with_io("read failed", io_err));
// or, if the failure is a generic file-system I/O outside the connection
// path, propagate via ? on Error::Io(io_err) directly.
```

**Generic state assertion** — before:
```rust
return Err(Error::new("connection already closed"));
```

after:
```rust
return Err(Error::internal("connection already closed"));
// Or, if recoverable (closed mid-operation), Error::Closed("...".into()).
```

**Pattern-match on `Error::Other`** — before:
```rust
if let Error::Other { message, .. } = &err { /* … */ }
```

after — the variant is gone. Match on the specific new variant the call site produces. Most former `Other` constructions are now `Error::Conversion`, `Error::Internal`, `Error::Config`, `Error::FeatureNotSupported`, or `Error::InvalidName`/`InvalidTableDefinition` based on the original message.

**Inspect the SQLSTATE of a server error** — `Error::sqlstate()` is preserved for backward-compatible inspection:
```rust
if err.sqlstate() == Some("23505") { /* duplicate-key path */ }
```

**Read SQLSTATE / detail / hint structurally** — new in v0.3.0:
```rust
if let Error::Server { sqlstate: Some(code), detail, hint, .. } = &err {
    log::warn!("server error {code}: detail={detail:?} hint={hint:?}");
}
```

### Notes for downstream crate authors

- The `From<hyperdb_api_core::client::Error> for hyperdb_api::Error` impl is exhaustive over `client::ErrorKind`. Adding a kind to `client::Error` will break this build until a mapping is added. This is intended.
- `Error::Connection { source }` carries an `Option<std::io::Error>`. The wire-protocol layer in `hyperdb-api-core` does not preserve typed causes through its boundary, so `source` is `None` for errors that originated there. Direct callers in `hyperdb-api` who construct `Error::connection_with_io` *do* preserve the typed source.
- The `Error::Internal { .. }` variant is a deliberate catch-all for invariant violations. New code should reach for a domain variant first.

---

## #70 (continued) — Ergonomic constructors across all workspace error types

The same ergonomic-constructor pattern was applied to every error type in the workspace that user code might construct, so call sites no longer need `.to_string()` ceremony for string-literal arguments.

### `hyperdb_api_salesforce::SalesforceAuthError`

New constructors, all taking `impl Into<String>`:

```rust
SalesforceAuthError::config(message);
SalesforceAuthError::private_key(message);
SalesforceAuthError::jwt(message);
SalesforceAuthError::http(message);
SalesforceAuthError::authorization(error_code, error_description);   // both impl Into<String>
SalesforceAuthError::token_exchange(message);
SalesforceAuthError::token_parse(message);
SalesforceAuthError::io(message);
```

`SalesforceAuthError::TokenExpired` is a unit variant with no constructor. Pattern-matching keeps PascalCase (`if let Err(SalesforceAuthError::Authorization { .. }) = result`). 26 internal call sites were rewritten.

### `hyperdb_bootstrap::Error`

New constructors:

```rust
Error::unsupported_platform(os, arch);                  // both impl Into<String>
Error::unknown_platform_slug(slug);
Error::io(context, source: std::io::Error);
Error::http_status(url, status: u16);
Error::curl_failed(url, code: i32);
Error::checksum_mismatch(expected, actual);             // both impl Into<String>
```

`Error::HyperdNotInArchive` (unit) and `Error::ScrapeFailed(&'static str)` already required no ceremony. The `#[from]`-generated `Http`/`TomlParse`/`Zip` variants take typed sources — no constructor needed. 26 call sites rewritten.

### `hyperdb_mcp::McpError`

Already ergonomic — `McpError::new(code: ErrorCode, message: impl Into<String>)` takes `impl Into<String>`. One residual `.to_string()` ceremony site was cleaned up; no new constructors needed.

### `hyperdb_api_core::client::Error`

Already ergonomic — its existing convenience constructors (`Error::connection`, `Error::query`, `Error::feature_not_supported`, `Error::other`, etc.) all take `impl Into<String>`. No changes required.

### What this means for callers

If you construct any of the workspace error types, drop the `.to_string()` / `.into()` from string-literal arguments:

```rust
// Before
Error::Conversion("NULL id".to_string())
SalesforceAuthError::Config("auth_mode is required".to_string())
hyperdb_bootstrap::Error::Io { context: "remove tmp".to_string(), source: e }

// After
hyperdb_api::Error::conversion("NULL id")
SalesforceAuthError::config("auth_mode is required")
hyperdb_bootstrap::Error::io("remove tmp", e)
```

`format!(...)` calls, owned `String` values, and `impl Display::to_string()` (where the source is not already `Into<String>`) all still work unchanged through the constructors.
