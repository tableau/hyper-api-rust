# Migrating to v0.3.0

This is the consolidated migration guide for the v0.3.0 bundle of breaking and additive changes. Each section corresponds to a bundle PR; the guide grows as each PR lands. The bundle ships as one major bump after the last PR merges.

> Each bundle PR uses `chore:` Conventional Commit prefix to defer release-please from cutting an early version. After all PRs merge, a single `feat:` commit with a `BREAKING CHANGE:` footer triggers v0.3.0.

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

## #69 — Transaction API consolidation

The raw transaction methods on `Connection` and `AsyncConnection` are now deprecated and hidden from rustdoc. The RAII guard at `Connection::transaction()` / `AsyncConnection::transaction()` is the recommended (and only documented) way to drive transactions.

### What's deprecated

```rust
Connection::begin_transaction(&self)        // -> #[doc(hidden)] #[deprecated]
Connection::commit(&self)                   // -> #[doc(hidden)] #[deprecated]
Connection::rollback(&self)                 // -> #[doc(hidden)] #[deprecated]
AsyncConnection::begin_transaction(&self)   // -> #[doc(hidden)] #[deprecated]
AsyncConnection::commit(&self)              // -> #[doc(hidden)] #[deprecated]
AsyncConnection::rollback(&self)            // -> #[doc(hidden)] #[deprecated]
```

These methods still exist and still work — your build will see compiler warnings rather than errors. They will be deleted in a future release; new code must use the RAII guard.

### Migration recipe

```rust
// Before
conn.begin_transaction()?;
conn.execute_command("INSERT INTO t VALUES (1)")?;
conn.commit()?;

// After (sync)
let txn = conn.transaction()?;          // requires &mut conn
txn.execute_command("INSERT INTO t VALUES (1)")?;
txn.commit()?;
```

For the async equivalent, the body of the function holding `conn` will need to take `&mut AsyncConnection` instead of `&AsyncConnection`. Where you previously had:

```rust
pub async fn ingest(conn: &AsyncConnection, ...) -> Result<(), McpError> {
    conn.begin_transaction().await?;
    ...
    conn.commit().await?;
}
```

write:

```rust
pub async fn ingest(conn: &mut AsyncConnection, ...) -> Result<(), McpError> {
    let txn = conn.transaction().await?;
    txn.execute_command("...").await?;
    txn.commit().await?;
}
```

Callers that hold a pooled connection (`deadpool::managed::Object<ConnectionManager>`) need `let mut conn = pool.get().await?;` and `&mut conn` at the call site.

### What didn't change

- `Connection::transaction(&mut self) -> Result<Transaction<'_>>` — kept as the canonical entry point.
- `Transaction::commit(self)` and `Transaction::rollback(self)` — kept; consume `self` to prevent double-commit.
- The `Drop for Transaction` auto-rollback safety net — kept.
- `AsyncTransaction` semantics, including the warning-only `Drop` (Rust has no async `Drop`) — kept.

### MCP follow-up

The MCP server's `Engine::execute_in_transaction` helper takes `&self` and so cannot use the RAII guard. It retains the deprecated raw methods with a function-level `#[allow(deprecated, reason = "...")]` annotation. Migrating it requires reshaping `Engine`'s locking model. Two structural paths and an acceptance-criteria checklist are written up in [issue #72](https://github.com/tableau/hyper-api-rust/issues/72).

---

## #61 + #62 — FromRow modernization

The `FromRow` trait was redesigned around a new [`RowAccessor`] type and a new [`#[derive(FromRow)]`][derive] proc-macro. The blanket tuple impls (1/2/3/4-tuple) were deleted; hand-written impls have a new signature.

[`RowAccessor`]: https://docs.rs/hyperdb-api/latest/hyperdb_api/struct.RowAccessor.html
[derive]: https://docs.rs/hyperdb-api/latest/hyperdb_api/derive.FromRow.html

### What's changed

| Surface | Before (v0.2.x) | After (v0.3.0) |
| ------- | --------------- | -------------- |
| `FromRow::from_row` signature | `fn from_row(row: &Row) -> Result<Self>` | `fn from_row(row: RowAccessor<'_>) -> Result<Self>` |
| Blanket tuple impls | `(Option<A>,)` … `(Option<A>, Option<B>, Option<C>, Option<D>)` | **Deleted.** Define a struct with `#[derive(FromRow)]` instead. |
| Derive macro | did not exist | `#[derive(FromRow)]` from the new `hyperdb-api-derive` crate (re-exported by `hyperdb-api`) |
| Name-based access on a single row | did not exist | `Row::get_by_name<T>(name)` |
| Cached column-name → index lookup | did not exist | `RowAccessor` carries one; built once per query in `fetch_*_as` |

### What's new

- **`#[derive(FromRow)]`** generates the `impl FromRow` for you. Field names match column names by default; `#[hyperdb(rename = "...")]` overrides the column name; `#[hyperdb(index = N)]` switches to positional access at column `N`. `Option<T>` fields use `get_opt` / `position_opt` (NULL → `None`); other fields use `get` / `position` (NULL → error). `rename` and `index` are mutually exclusive.

  ```rust
  use hyperdb_api::FromRow;

  #[derive(FromRow)]
  struct User {
      id: i32,
      name: String,
      #[hyperdb(rename = "email_address")]
      email: Option<String>,
  }

  // Useful for queries with computed/unnamed columns, e.g.
  // `SELECT id, COUNT(*) FROM ... GROUP BY id`.
  #[derive(FromRow)]
  struct Aggregate {
      #[hyperdb(index = 0)]
      id: i32,
      #[hyperdb(index = 1)]
      total: Option<i64>,
  }
  ```

- **`RowAccessor<'a>`** is the parameter type of the new `FromRow::from_row`. It exposes:
  - `get<T>(name: &str) -> Result<T>` — required field; missing/NULL/type-mismatch return `Error::Column`.
  - `get_opt<T>(name: &str) -> Result<Option<T>>` — optional field; NULL becomes `None`.
  - `position<T>(idx: usize) -> Result<T>` — positional access; out-of-range returns `Error::ColumnIndexOutOfBounds`.
  - `position_opt<T>(idx: usize) -> Result<Option<T>>` — positional access; NULL becomes `None`.

- **`Row::get_by_name<T>(name)`** does the same name-based lookup but on a single `Row` (no cached lookup map). Convenient for hand-coded paths that don't go through `FromRow`. Doc warns that it's a linear scan; recommends `#[derive(FromRow)]` or `fetch_*_as` for hot paths.

### Migration recipes

#### Hand-written `FromRow` impl

```rust
// Before
impl FromRow for User {
    fn from_row(row: &Row) -> Result<Self> {
        Ok(User {
            id: row.get::<i32>(0).ok_or_else(|| Error::conversion("NULL id"))?,
            name: row.get::<String>(1).unwrap_or_default(),
        })
    }
}

// After
impl FromRow for User {
    fn from_row(row: RowAccessor<'_>) -> Result<Self> {
        Ok(User {
            id: row.get("id")?,
            name: row.get_opt("name")?.unwrap_or_default(),
        })
    }
}
```

The new shape is shorter, more readable, and decouples your code from column position — reordering `SELECT` columns no longer breaks your impl.

#### Tuple destructuring (deleted)

```rust
// Before — blanket tuple impl
let row = conn.fetch_one("SELECT id, name FROM users")?;
let (id, name): (Option<i32>, Option<String>) = FromRow::from_row(&row)?;

// After — define a struct
#[derive(FromRow)]
struct User { id: Option<i32>, name: Option<String> }
let user: User = conn.fetch_one_as("SELECT id, name FROM users")?;
```

Or, if you really want positional access without a struct, use `Row::get(idx)` directly:

```rust
let row = conn.fetch_one("SELECT id, name FROM users")?;
let id: Option<i32> = row.get(0);
let name: Option<String> = row.get(1);
```

#### Direct `T::from_row(&row)` calls

If you previously called `T::from_row(&row)` directly (outside `fetch_*_as`), the new signature requires a `RowAccessor`. Easiest migration: use `fetch_one_as` / `fetch_all_as` instead, which build the cached lookup for you.

If you must construct a `RowAccessor` yourself (e.g. processing rows from a custom source), the constructor is `pub(crate)`. File an issue if you need this surfaced — current direction is to keep `RowAccessor` construction internal so the cache lifetime stays tied to `fetch_*_as`.

### Errors

The derive and `RowAccessor` accessors return `Error::Column { name, kind }` for column-access failures, where `ColumnErrorKind` is one of:

- `Missing` — column with that name not in the result schema
- `Null` — required field, but the cell is SQL `NULL`
- `TypeMismatch { expected, actual }` — the cell value couldn't be decoded as `T`

`Error::ColumnIndexOutOfBounds { idx, column_count }` is returned by `RowAccessor::position` when `idx` is out of range.

These variants were shipped in `#70` so this PR doesn't re-break the error type.

### Performance note

`fetch_*_as` builds a `HashMap<&str, usize>` once per query (O(N) in the column count). Each row's `RowAccessor::get(name)` then runs a single hash lookup followed by typed access — O(1) per field per row. This is strictly better than the previous behavior, where a hand-written impl using `try_get(idx, name)` had to know column positions hard-coded.

For one-off named access on a `Row` outside `fetch_*_as`, `Row::get_by_name` is a linear scan over `ResultSchema::column_index`. For hot paths (many rows × many fields), prefer `#[derive(FromRow)]`.

### `hyperdb-api-derive` crate

The proc-macro lives in a new `hyperdb-api-derive` workspace crate (Rust requires proc-macro code to live in its own `proc-macro = true` crate). It's re-exported from `hyperdb-api`, so callers don't need a direct dependency — same pattern as serde / thiserror. **Don't add `hyperdb-api-derive` to your `Cargo.toml`**; just `use hyperdb_api::FromRow;`.

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
