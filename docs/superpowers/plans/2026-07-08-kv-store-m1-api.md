# KV Store (M1 — Rust API) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an ergonomic, typed key-value store (`KvStore` / `AsyncKvStore`) to `hyperdb-api`, backed by a single fixed Hyper SQL table, with sync + async twins and performance benchmarks.

**Architecture:** A companion struct borrowing `&'conn Connection` (mirroring `Catalog`/`Inserter`), namespacing every named store by a `store_name` column in one fixed table `_hyperdb_kv_store`. Writes use the crate's parameterized extended-query path (`command_params`/`query_params`); `set` is an UPDATE-then-conditional-INSERT upsert (Hyper has no `ON CONFLICT`); `pop` and `set_batch` wrap multiple statements in a transaction via the crate-internal `begin/commit/rollback_raw` methods.

**Tech Stack:** Rust, `hyperdb-api` (pure-Rust Hyper client), `serde`/`serde_json` (already direct deps), a real `hyperd` subprocess for integration tests (`HyperProcess::new`).

## Global Constraints

Every task's requirements implicitly include this section. Values copied verbatim from `docs/superpowers/specs/2026-07-08-kv-store-design.md`, adjusted by two corrections verified against source (noted below).

- **PR title uses a `feat:` prefix** — this is the real feature (M1). M2 (MCP) is a separate branch/plan with a `fix:` prefix; **do not touch `hyperdb-mcp` in M1.**
- **Backing table (fixed, static) — NO `PRIMARY KEY`:**
  ```sql
  CREATE TABLE IF NOT EXISTS _hyperdb_kv_store (
      store_name TEXT NOT NULL,
      key        TEXT NOT NULL,
      value      TEXT
  );
  ```
  Table name is `_hyperdb_kv_store` (the `_hyperdb_` prefix so M2's `is_internal_table()` auto-hides it).

  **⚠️ CRITICAL — verified empirically (2026-07-08).** Hyper **rejects** a `PRIMARY KEY`
  clause at `CREATE TABLE` time with `0A000: Index support is disabled` — the exact behavior
  documented in `hyperdb-mcp/src/table_catalog.rs:54-58`, which the plan mirrors and which
  therefore declares **no** PK. A probe against the pinned `hyperd` confirmed: `CREATE TABLE …
  PRIMARY KEY (…)` fails; the same table without a PK creates cleanly; and the
  UPDATE-then-conditional-`INSERT … WHERE NOT EXISTS` idiom enforces per-`(store_name, key)`
  uniqueness application-side (duplicate conditional INSERT affected **0 rows**). **Uniqueness is
  an application-side invariant, not an engine constraint.** Every `CREATE TABLE` in this plan
  (both twins, plus the `kv_list_stores` guards) omits the `PRIMARY KEY` clause.
- **Name validation:** `store_name` and `key` must be **non-empty**, match ASCII `[A-Za-z0-9_.-]+` (ASCII alphanumeric, `_`, `.`, `-`), and be **at most 512 bytes**. Violations → `Error::invalid_name`. Applied to `store_name` at `kv_store(name)`, to `key` on every keyed call. Both the max length (`KV_MAX_NAME_BYTES`) and the human-readable charset (`KV_CHARSET`) are named `const`s per M-DOCUMENTED-MAGIC.
- **New error variant:** `Error::Serialization(String)` with a public constructor `Error::serialization(...)`, for `get_as`/`set_as` JSON failures. Reuse existing variants otherwise (`invalid_name`, `feature_not_supported`, `Server`). Do **not** introduce a separate error enum (M-APP-ERROR / M-ERRORS-CANONICAL-STRUCTS).
- **Transport gating:** all KV methods use parameterized queries (`query_params`/`command_params`), which already return `Error::feature_not_supported` on gRPC. No extra gating code is required; document it in `# Errors`.
- **No narrowing `as` casts on integers** (repo rule #7). `size()` returns the `COUNT(*)` `i64` directly. Any width conversion uses `TryFrom` or a justified `#[expect(clippy::cast_*, reason = "...")]`.
- **Lints are `-D warnings`:** `missing_docs`, `missing_debug_implementations`, clippy `pedantic`+`cargo`, `cast_possible_truncation`/`cast_sign_loss`/`cast_possible_wrap` = deny, `allow_attributes_without_reason` = warn. Every `#[expect]`/`#[allow]` carries a `reason = "..."`. Every public type derives `Debug` (M-PUBLIC-DEBUG). Every `pub` item has a `///` summary < 15 words (M-CANONICAL-DOCS / M-FIRST-DOC-SENTENCE), with `# Examples` (`no_run`), `# Errors`, and `# Panics` where applicable.
- **Testing gate:** fast loop is **`make test-api`** (API only, no MCP/Node — a real Makefile target). Tests start a real `hyperd` via `HyperProcess::new(None, Some(&params))` — **never invent `hyperd` flags** (repo rule #9). **Never report a test as passing without seeing real output**; a silent hang (~30s no output) is a failure, not a pass (repo rule #10). Run `cargo fmt` + `cargo clippy` before every commit. Commit with explicit `git add <files>` (never `-A`).
- **Docs to update (Task 10):** `hyperdb-api/README.md` (overview entry + KV sub-section), `hyperdb-api/CHANGELOG.md` (`### Added` under `## [Unreleased]`), `hyperdb-api/DEVELOPMENT.md` ("Features Implemented"). Confirm `RUSTDOCFLAGS="-D warnings" cargo doc` is clean.

### Two spec corrections (verified against source — supersede the spec where they conflict)

1. **`serde` + `serde_json` are already direct deps** of `hyperdb-api` (`Cargo.toml:47-48`, used by `query_stats`). The spec's "add dependencies" step is a **no-op** — do not add them again. `serde` has the `derive` feature at the workspace level (`Cargo.toml:65`).
2. **Parameters are NOT an "escaped-literal facade."** The spec (§136-146) claims `query_params`
   "convert positional params to safely-escaped SQL literals before sending" — **this is wrong.**
   `command_params`/`query_params` use the **real** PostgreSQL extended-query protocol (Parse/Bind/
   Execute with binary `HyperBinary` params — `connection.rs:1204-1230`, `async_connection.rs:718-769`).
   The plan's correction supersedes the spec on this point. Repeated `$N` placeholders are therefore
   protocol-safe, but to remove **all** doubt this plan uses **distinct** placeholders in the
   conditional INSERT (`$4`/`$5` instead of reusing `$1`/`$2`), passing the repeated values
   positionally. Both reviewers independently verified this against source.

3. **`PRIMARY KEY` at `CREATE TABLE` is rejected by Hyper** (`0A000: Index support is disabled`),
   verified empirically against the pinned `hyperd` on 2026-07-08 and documented in
   `hyperdb-mcp/src/table_catalog.rs:54-58`. The spec's backing-table DDL (which included a PK) and
   this plan's earlier draft are both corrected: the table has **no** `PRIMARY KEY`, and uniqueness
   is enforced application-side by the conditional-INSERT idiom (probe: duplicate insert → 0 rows).

### Verified building blocks (call these; do not invent APIs)

Sync (`Connection`, in `connection.rs`):
- `execute_command(&self, &str) -> Result<u64>`
- `execute_query(&self, &str) -> Result<Rowset<'_>>`
- `query_params(&self, query: &str, params: &[&dyn ToSqlParam]) -> Result<Rowset<'_>>` — TCP-only
- `command_params(&self, query: &str, params: &[&dyn ToSqlParam]) -> Result<u64>` — TCP-only, returns affected rows
- `pub(crate) begin_transaction_raw(&self)` / `commit_raw(&self)` / `rollback_raw(&self)` — take `&self` (the escape hatch; `transaction()` needs `&mut self` and cannot be used from a shared borrow)

Sync results (`result.rs`):
- `Rowset::first_row(self) -> Result<Option<Row>>` — **`None` on empty, no error** (use for `get`/`pop`/`exists`)
- `Rowset::scalar<T: RowValue>(self) -> Result<Option<T>>` — **errors on zero rows** (use only for `COUNT(*)`, which always returns a row)
- `Rowset::next_chunk(&mut self) -> Result<Option<Vec<Row>>>` — streaming (use for `keys`/`kv_list_stores`)
- `Row::get<T: RowValue>(&self, idx: usize) -> Option<T>` — `None` on SQL NULL

Async twins (`async_connection.rs` / `async_result.rs`): identical names, `.await`; `AsyncConnection`, `AsyncRowset`. `AsyncRowset::first_row(self)`, `.scalar()`, `.next_chunk(&mut self)` all `async`.

Param binding: `&[&x, &y]` where each `x: &str`/`String`/`i64` etc. (`params.rs` impls). Pattern mirrors `conn.query_params("... = $1", &[&user_input])`.

Borrow pattern to mirror (`catalog.rs:57-66`): `#[derive(Debug)] pub struct Catalog<'conn> { connection: &'conn Connection }` + `pub fn new(connection: &'conn Connection) -> Self`.

Test harness (`hyperdb-api/tests/common/mod.rs`): `TestConnection::new()` (sync). Async tests use the local `fresh_async_conn` idiom from `tests/async_transaction_tests.rs` (`HyperProcess::new` → `require_endpoint()` → `AsyncConnection::connect`).

---

## File Structure

- **Create** `hyperdb-api/src/kv_store.rs` — sync `KvStore<'conn>`, `Connection::kv_store`/`kv_list_stores`, shared `pub(crate)` constants + `validate_kv_name` + SQL, unit tests for validation.
- **Create** `hyperdb-api/src/async_kv_store.rs` — async `AsyncKvStore<'conn>`, `AsyncConnection::kv_store`/`kv_list_stores` (reuses `kv_store::{validate_kv_name, constants}`).
- **Modify** `hyperdb-api/src/error.rs` — add `Serialization` variant + `serialization()` constructor + test.
- **Modify** `hyperdb-api/src/lib.rs` — `mod kv_store; mod async_kv_store;` + `pub use kv_store::KvStore; pub use async_kv_store::AsyncKvStore;` + a `compile_fail` lifetime doc test.
- **Create** `hyperdb-api/tests/kv_store_tests.rs` — sync integration tests.
- **Create** `hyperdb-api/tests/async_kv_store_tests.rs` — async integration tests.
- **Create** `hyperdb-api/benches/kv_benchmark.rs` — single-commit vs batched-commit perf benchmark.
- **Modify** `hyperdb-api/Cargo.toml` — register the `kv_benchmark` example.
- **Modify** `hyperdb-api/README.md`, `hyperdb-api/CHANGELOG.md`, `hyperdb-api/DEVELOPMENT.md` — docs.

---

## Task 1: Add `Error::Serialization` variant

**Files:**
- Modify: `hyperdb-api/src/error.rs`

**Interfaces:**
- Produces: `Error::Serialization(String)` variant; `Error::serialization(message: impl Into<String>) -> Self`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `error.rs`:

```rust
#[test]
fn serialization_constructor_round_trip() {
    let err = Error::serialization("expected value at line 1 column 1");
    assert_eq!(
        err.to_string(),
        "serialization error: expected value at line 1 column 1"
    );
    assert!(matches!(err, Error::Serialization(_)));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --lib error::tests::serialization_constructor_round_trip`
Expected: FAIL — `no variant named Serialization` / `no function named serialization`.

- [ ] **Step 3: Add the variant and constructor**

In the `enum Error` block, after the `Conversion` variant (keep the `// ---- Type / value ----` grouping), add:

```rust
    /// Serialization or deserialization of a value failed (e.g. a
    /// `get_as`/`set_as` JSON conversion). Distinct from
    /// [`Self::Conversion`], which covers SQL type/binary decoding.
    #[error("serialization error: {0}")]
    Serialization(String),
```

In the `impl Error` block, near the other tuple-variant constructors (after `conversion`), add:

```rust
    /// Constructs an [`Self::Serialization`] error.
    pub fn serialization(message: impl Into<String>) -> Self {
        Error::Serialization(message.into())
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --lib error::tests::serialization_constructor_round_trip`
Expected: PASS.

- [ ] **Step 5: Verify `sqlstate()` still compiles (new variant falls into `_ => None`)**

Run: `cargo build -p hyperdb-api`
Expected: clean build. (`sqlstate()` has a `_ => None` arm; `Serialization` needs no change there.)

- [ ] **Step 6: Commit**

```bash
cargo fmt -p hyperdb-api
git add hyperdb-api/src/error.rs
git commit -m "feat(kv): add Error::Serialization variant for get_as/set_as"
```

---

## Task 2: Name validation + shared constants

**Files:**
- Create: `hyperdb-api/src/kv_store.rs` (initial: constants + validator + unit tests only)

**Interfaces:**
- Produces:
  - `pub(crate) const KV_TABLE: &str = "_hyperdb_kv_store";`
  - `pub(crate) const KV_MAX_NAME_BYTES: usize = 512;`
  - `pub(crate) const KV_CHARSET: &str = "A-Z a-z 0-9 _ . -";`
  - `pub(crate) fn validate_kv_name(name: &str, kind: &str) -> Result<()>` — used by both sync and async KV code.
  - `pub(crate) fn kv_create_table_sql(table_ref: &str) -> String` — the single source of truth for the backing-table DDL (no `PRIMARY KEY`), used by both sync/async `open` and both `kv_list_stores` guards so the twins can never diverge on the DDL.

- [ ] **Step 1: Create the file with constants, validator, and failing unit tests**

Create `hyperdb-api/src/kv_store.rs`:

```rust
// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Key-value store over a fixed Hyper table.
//!
//! [`KvStore`] is an ergonomic string-native KV abstraction backed by a
//! single table, [`KV_TABLE`], namespaced by a `store_name` column. Every
//! named store shares that table; a handle binds one store name, validated
//! once at [`Connection::kv_store`](crate::Connection::kv_store).
//!
//! Hyper has no native KV store and no `ON CONFLICT`/`MERGE`; `set` is an
//! `UPDATE`-then-conditional-`INSERT` upsert. See the crate `DEVELOPMENT.md`
//! for the design rationale.

use crate::error::{Error, Result};

/// Fixed backing table for every named KV store.
///
/// The `_hyperdb_` prefix matches the crate's internal-table convention so
/// downstream tooling can auto-hide it from schema listings.
pub(crate) const KV_TABLE: &str = "_hyperdb_kv_store";

/// Maximum length, in bytes, of a store name or key.
pub(crate) const KV_MAX_NAME_BYTES: usize = 512;

/// Human-readable description of the allowed store-name/key charset.
///
/// Used in validation error messages so the allowed set is stated in one
/// place (M-DOCUMENTED-MAGIC) rather than duplicated as a string literal.
pub(crate) const KV_CHARSET: &str = "A-Z a-z 0-9 _ . -";

/// Validates a store name or key: non-empty, ASCII `[A-Za-z0-9_.-]+`, `<= 512` bytes.
///
/// `kind` labels the value in the error message (`"store name"` / `"key"`).
///
/// # Errors
///
/// Returns [`Error::InvalidName`] if `name` is empty, exceeds
/// [`KV_MAX_NAME_BYTES`] bytes, or contains a byte outside the ASCII
/// [`KV_CHARSET`] (`A-Z a-z 0-9 _ . -`).
pub(crate) fn validate_kv_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid_name(format!("KV {kind} must not be empty")));
    }
    if name.len() > KV_MAX_NAME_BYTES {
        return Err(Error::invalid_name(format!(
            "KV {kind} exceeds {KV_MAX_NAME_BYTES}-byte limit ({} bytes)",
            name.len()
        )));
    }
    if let Some(bad) = name
        .bytes()
        .find(|&b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-'))
    {
        return Err(Error::invalid_name(format!(
            "KV {kind} contains an invalid byte {bad:#04x}; allowed: {KV_CHARSET}"
        )));
    }
    Ok(())
}

/// Builds the `CREATE TABLE IF NOT EXISTS` DDL for the KV backing table.
///
/// Single source of truth for the schema, shared by the sync and async
/// constructors and both `kv_list_stores` guards. The table has **no**
/// `PRIMARY KEY`: Hyper rejects one at create time (`0A000: Index support is
/// disabled`, see `hyperdb-mcp/src/table_catalog.rs`), so per-`(store_name,
/// key)` uniqueness is an application-side invariant enforced by the
/// UPDATE-then-conditional-INSERT upsert, not an engine constraint.
pub(crate) fn kv_create_table_sql(table_ref: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table_ref} \
         (store_name TEXT NOT NULL, key TEXT NOT NULL, value TEXT)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for ok in ["a", "store_1", "my.key-2", "A", &"z".repeat(KV_MAX_NAME_BYTES)] {
            assert!(validate_kv_name(ok, "key").is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn rejects_empty() {
        let err = validate_kv_name("", "store name").unwrap_err();
        assert!(matches!(err, Error::InvalidName(_)));
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(KV_MAX_NAME_BYTES + 1);
        let err = validate_kv_name(&long, "key").unwrap_err();
        assert!(matches!(err, Error::InvalidName(_)));
        assert!(err.to_string().contains("byte limit"));
    }

    #[test]
    fn rejects_bad_charset() {
        for bad in ["a b", "a/b", "a'b", "a\"b", "a;b", "naïve", "a\0b"] {
            let err = validate_kv_name(bad, "key").unwrap_err();
            assert!(matches!(err, Error::InvalidName(_)), "should reject {bad:?}");
        }
    }
}
```

- [ ] **Step 2: Register the module (temporarily) so the file compiles**

In `hyperdb-api/src/lib.rs`, alongside the other `mod` declarations (e.g. after `mod inserter;`), add:

```rust
mod kv_store;
```

(The `pub use` for `KvStore` is added in Task 3, once the type exists.)

- [ ] **Step 3: Run the unit tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --lib kv_store::tests`
Expected: PASS (4 tests). These are pure functions — no `hyperd` needed, but the env var keeps the command uniform.

- [ ] **Step 4: Clippy + fmt**

Run: `cargo clippy -p hyperdb-api --all-targets && cargo fmt -p hyperdb-api`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add hyperdb-api/src/kv_store.rs hyperdb-api/src/lib.rs
git commit -m "feat(kv): add KV name validator and shared constants"
```

---

## Task 3: `KvStore` scaffolding + `Connection::kv_store` + PK-enforcement probe

**Files:**
- Modify: `hyperdb-api/src/kv_store.rs`
- Modify: `hyperdb-api/src/lib.rs` (add `pub use kv_store::KvStore;`)
- Create: `hyperdb-api/tests/kv_store_tests.rs`

**Interfaces:**
- Consumes: `KV_TABLE`, `validate_kv_name` (Task 2); `Connection::{execute_command, query_params}` and streaming (`connection.rs`).
- Produces:
  - `pub struct KvStore<'conn>` (holds `&'conn Connection`, validated `store_name: String`, `table_ref: String`).
  - `impl Connection { pub fn kv_store(&self, name: &str) -> Result<KvStore<'_>>; pub fn kv_list_stores(&self) -> Result<Vec<String>>; }`
  - `impl KvStore<'conn> { pub fn name(&self) -> &str; pub(crate) fn with_target(conn, name, target) -> Result<Self>; }`

**Design note — `table_ref` seam for M2.** `KvStore` stores a `table_ref: String` computed once at construction. `kv_store()` sets it to the bare `KV_TABLE`; the `pub(crate) with_target` constructor (used later by M2) sets it to a database/schema-qualified, escaped reference. All SQL formats `{self.table_ref}` (a trusted, construction-time string) while keeping `store_name`/`key`/`value` as bound `$N` params. This satisfies M2 without a public API change; M1's public surface is only `kv_store(name)`.

- [ ] **Step 1: Write the failing integration tests**

Create `hyperdb-api/tests/kv_store_tests.rs`:

```rust
// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for the sync [`KvStore`] API.

mod common;

use common::TestConnection;
use hyperdb_api::{Error, Result};

#[test]
fn open_store_creates_backing_table() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    assert_eq!(kv.name(), "cfg");
    // Backing table exists and is initially empty for this store. Checked via a
    // direct COUNT (not `size()`, which arrives in Task 5) so Task 3 is
    // self-contained and compiles on its own.
    let count = tc
        .connection
        .execute_query("SELECT COUNT(*) FROM _hyperdb_kv_store WHERE store_name = 'cfg'")?
        .scalar::<i64>()?;
    assert_eq!(count, Some(0));
    Ok(())
}

#[test]
fn rejects_invalid_store_name() {
    let tc = TestConnection::new().unwrap();
    let err = tc.connection.kv_store("bad name").unwrap_err();
    assert!(matches!(err, Error::InvalidName(_)));
}

/// Documents the engine's duplicate-row behavior on the (PK-less) backing table.
/// The KV upsert guarantees single-row-per-key application-side; this test
/// records what the pinned `hyperd` does with a raw duplicate `INSERT` so
/// expectations stay honest and prove why the app-side upsert is required.
///
/// Empirically (2026-07-08) the table has NO `PRIMARY KEY` (Hyper rejects one:
/// `0A000: Index support is disabled`), so a raw duplicate insert is ACCEPTED —
/// which is exactly why `set` must use the conditional-INSERT idiom, not a bare
/// `INSERT`.
#[test]
fn documents_duplicate_insert_behavior() -> Result<()> {
    let tc = TestConnection::new()?;
    let _ = tc.connection.kv_store("dup_probe")?; // ensure table exists
    tc.connection.execute_command(
        "INSERT INTO _hyperdb_kv_store (store_name, key, value) VALUES ('dup_probe', 'k', 'v1')",
    )?;
    let dup = tc.connection.execute_command(
        "INSERT INTO _hyperdb_kv_store (store_name, key, value) VALUES ('dup_probe', 'k', 'v2')",
    );
    match dup {
        Err(e) => eprintln!("duplicate raw INSERT rejected -> {e}"),
        Ok(n) => eprintln!("duplicate raw INSERT accepted ({n} row); app-side upsert required"),
    }
    Ok(())
}
```

- [ ] **Step 2: Run to verify failure**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests`
Expected: FAIL — `no method named kv_store` on `Connection`. (The test uses only `kv_store`,
`name`, `execute_command`, and `execute_query().scalar()`, all of which exist except `kv_store`
itself — so once Step 3 adds `kv_store`, Task 3 compiles and passes with no later edits.)

- [ ] **Step 3: Implement the struct and constructors in `kv_store.rs`**

Add above the `#[cfg(test)]` block:

```rust
use crate::connection::Connection;

/// A handle to one named key-value store, backed by [`KV_TABLE`].
///
/// Borrows its [`Connection`] for the handle's lifetime (`'conn`), matching
/// the crate's [`Catalog`](crate::Catalog)/[`Inserter`](crate::Inserter)
/// borrow convention. Open one with
/// [`Connection::kv_store`](crate::Connection::kv_store).
///
/// # Examples
///
/// ```no_run
/// use hyperdb_api::{Connection, CreateMode, Result};
///
/// fn main() -> Result<()> {
///     let conn = Connection::connect("localhost:7483", "app.hyper", CreateMode::CreateIfNotExists)?;
///     let kv = conn.kv_store("settings")?;
///     kv.set("theme", "dark")?;
///     assert_eq!(kv.get("theme")?, Some("dark".to_string()));
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct KvStore<'conn> {
    connection: &'conn Connection,
    store_name: String,
    table_ref: String,
}

impl<'conn> KvStore<'conn> {
    /// Opens a handle to `name`, creating [`KV_TABLE`] if needed.
    fn open(connection: &'conn Connection, name: &str, table_ref: String) -> Result<Self> {
        validate_kv_name(name, "store name")?;
        connection.execute_command(&kv_create_table_sql(&table_ref))?;
        Ok(KvStore {
            connection,
            store_name: name.to_string(),
            table_ref,
        })
    }

    /// Opens a handle to a store in the default location.
    pub(crate) fn new(connection: &'conn Connection, name: &str) -> Result<Self> {
        Self::open(connection, name, KV_TABLE.to_string())
    }

    /// Opens a handle targeting an explicit, already-escaped table reference.
    ///
    /// Crate-internal seam for the MCP milestone (routes into an attached
    /// database). `target` is interpolated directly into SQL, so the **caller
    /// must supply a pre-validated / identifier-escaped, SQL-safe qualifier**
    /// (M2 must escape it via the crate's identifier-quoting before calling —
    /// `store_name`/`key`/`value` are always bound params, but `target` is not).
    #[allow(
        dead_code,
        reason = "M2 (hyperdb-mcp) consumer; kept here so M1 needs no later API change"
    )]
    pub(crate) fn with_target(
        connection: &'conn Connection,
        name: &str,
        target: &str,
    ) -> Result<Self> {
        Self::open(connection, name, format!("{target}.{KV_TABLE}"))
    }

    /// Returns this store's validated name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.store_name
    }
}
```

Add the `Connection` inherent methods. Put them in `kv_store.rs` (inherent impls can live in any module of the defining crate):

```rust
impl Connection {
    /// Opens a handle to a named key-value store, creating the table if needed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, CreateMode, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let kv = conn.kv_store("session")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `name` is empty, too long, or has invalid characters.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the `CREATE TABLE IF NOT EXISTS` fails.
    pub fn kv_store(&self, name: &str) -> Result<KvStore<'_>> {
        KvStore::new(self, name)
    }

    /// Lists the names of every KV store that currently holds at least one key.
    ///
    /// Creates the backing table first (via [`kv_create_table_sql`]) so calling
    /// this on a fresh database returns an empty list rather than erroring on a
    /// missing table.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the query fails.
    pub fn kv_list_stores(&self) -> Result<Vec<String>> {
        self.execute_command(&kv_create_table_sql(KV_TABLE))?;
        let mut result = self.execute_query(&format!(
            "SELECT DISTINCT store_name FROM {KV_TABLE} ORDER BY store_name ASC"
        ))?;
        let mut names = Vec::new();
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                if let Some(name) = row.get::<String>(0) {
                    names.push(name);
                }
            }
        }
        Ok(names)
    }
}
```

- [ ] **Step 4: Add `pub use` for `KvStore`**

In `lib.rs`, add near the other `pub use`s:

```rust
pub use kv_store::KvStore;
```

(No test edit is needed — the Task-3 test in Step 1 never calls `size()`; it checks emptiness
via `execute_query` + `scalar::<i64>()`, so Task 3 compiles and passes on its own. `size()` gets
its own dedicated test in Task 5. This removes the fragile "stand-in then restore" churn the plan
review flagged.)

- [ ] **Step 5: Run the tests**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests -- --nocapture`
Expected: PASS (3 tests). Read the `--nocapture` output for the PK-probe `eprintln!` line and record the observed behavior in the commit message. (Empirically, the pinned `hyperd` accepts a duplicate raw `INSERT` because the table has no PK — the probe documents this and proves why the app-side conditional-INSERT upsert is required.)

- [ ] **Step 6: Clippy + fmt, then commit**

```bash
cargo clippy -p hyperdb-api --all-targets && cargo fmt -p hyperdb-api
git add hyperdb-api/src/kv_store.rs hyperdb-api/src/lib.rs hyperdb-api/tests/kv_store_tests.rs
git commit -m "feat(kv): add KvStore scaffolding, kv_store/kv_list_stores, PK probe"
```

---

## Task 4: `get` / `set` (upsert) + `get_as` / `set_as`

**Files:**
- Modify: `hyperdb-api/src/kv_store.rs`
- Modify: `hyperdb-api/tests/kv_store_tests.rs`

**Interfaces:**
- Consumes: `Connection::{command_params, query_params}`; `Rowset::first_row`; `Row::get`.
- Produces on `KvStore<'conn>`:
  - `pub fn get(&self, key: &str) -> Result<Option<String>>`
  - `pub fn set(&self, key: &str, value: &str) -> Result<()>`
  - `pub fn get_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>>`
  - `pub fn set_as<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()>`

- [ ] **Step 1: Write failing tests**

Append to `kv_store_tests.rs`:

```rust
use serde::{Deserialize, Serialize};

#[test]
fn set_then_get_and_overwrite() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    assert_eq!(kv.get("missing")?, None);
    kv.set("k", "v1")?;
    assert_eq!(kv.get("k")?, Some("v1".to_string()));
    kv.set("k", "v2")?; // upsert overwrite
    assert_eq!(kv.get("k")?, Some("v2".to_string()));
    Ok(())
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Profile {
    name: String,
    level: u32,
}

#[test]
fn set_as_get_as_round_trip() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    let p = Profile { name: "ada".into(), level: 7 };
    kv.set_as("profile", &p)?;
    assert_eq!(kv.get_as::<Profile>("profile")?, Some(p));
    assert_eq!(kv.get_as::<Profile>("absent")?, None);
    Ok(())
}

#[test]
fn get_as_malformed_json_is_serialization_error() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    kv.set("bad", "not json")?;
    let err = kv.get_as::<Profile>("bad").unwrap_err();
    assert!(matches!(err, Error::Serialization(_)));
    Ok(())
}

#[test]
fn set_rejects_invalid_key() {
    let tc = TestConnection::new().unwrap();
    let kv = tc.connection.kv_store("cfg").unwrap();
    assert!(matches!(kv.set("bad key", "v"), Err(Error::InvalidName(_))));
    assert!(matches!(kv.get("bad key"), Err(Error::InvalidName(_))));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests`
Expected: FAIL — `no method named get`/`set`/`get_as`/`set_as`.

- [ ] **Step 3: Implement the methods in `KvStore`'s impl block**

```rust
    /// Returns the value for `key`, or `None` if the key is absent or NULL.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the query fails.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        validate_kv_name(key, "key")?;
        let sql = format!(
            "SELECT value FROM {} WHERE store_name = $1 AND key = $2",
            self.table_ref
        );
        // Bind store_name/key as `&str` params (never interpolated) — uniform
        // `&str` element types coerce cleanly to `&[&dyn ToSqlParam]`.
        let row = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str(), &key])?
            .first_row()?;
        Ok(row.and_then(|r| r.get::<String>(0)))
    }

    /// Sets `key` to `value`, inserting or overwriting (upsert).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the `UPDATE`/`INSERT` fails.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        validate_kv_name(key, "key")?;
        self.upsert(key, value)
    }

    /// UPDATE-then-conditional-INSERT upsert. Assumes `key` is validated.
    ///
    /// Hyper has no `ON CONFLICT`; this mirrors the proven `_table_catalog`
    /// idiom. The conditional INSERT uses distinct placeholders (`$4`/`$5`)
    /// so it is unambiguous under the extended-query protocol.
    fn upsert(&self, key: &str, value: &str) -> Result<()> {
        let store = self.store_name.as_str();
        let updated = self.connection.command_params(
            &format!(
                "UPDATE {} SET value = $3 WHERE store_name = $1 AND key = $2",
                self.table_ref
            ),
            &[&store, &key, &value],
        )?;
        if updated == 0 {
            self.connection.command_params(
                &format!(
                    "INSERT INTO {t} (store_name, key, value) \
                     SELECT $1, $2, $3 \
                     WHERE NOT EXISTS (SELECT 1 FROM {t} WHERE store_name = $4 AND key = $5)",
                    t = self.table_ref
                ),
                &[&store, &key, &value, &store, &key],
            )?;
        }
        Ok(())
    }

    /// Deserializes the JSON-encoded value for `key` into `T`.
    ///
    /// Returns `None` if the key is absent.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::Serialization`] if the stored value is not valid JSON for `T`.
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`] as for [`get`](Self::get).
    pub fn get_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key)? {
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| Error::serialization(e.to_string())),
            None => Ok(None),
        }
    }

    /// Serializes `value` to JSON and stores it under `key` (upsert).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::Serialization`] if `value` cannot be serialized to JSON.
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`] as for [`set`](Self::set).
    pub fn set_as<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        validate_kv_name(key, "key")?;
        let json = serde_json::to_string(value).map_err(|e| Error::serialization(e.to_string()))?;
        self.upsert(key, &json)
    }
```

- [ ] **Step 4: Run the tests**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests`
Expected: PASS (all Task-3 + Task-4 tests). This also empirically confirms the distinct-placeholder upsert against real `hyperd` (correction #2).

- [ ] **Step 5: Clippy + fmt, then commit**

```bash
cargo clippy -p hyperdb-api --all-targets && cargo fmt -p hyperdb-api
git add hyperdb-api/src/kv_store.rs hyperdb-api/tests/kv_store_tests.rs
git commit -m "feat(kv): add get/set upsert and serde get_as/set_as"
```

---

## Task 5: `delete` / `exists` / `size` / `keys` / `clear` + empty `kv_list_stores`

**Files:**
- Modify: `hyperdb-api/src/kv_store.rs`
- Modify: `hyperdb-api/tests/kv_store_tests.rs`

**Interfaces:**
- Produces on `KvStore<'conn>`:
  - `pub fn delete(&self, key: &str) -> Result<bool>`
  - `pub fn exists(&self, key: &str) -> Result<bool>`
  - `pub fn size(&self) -> Result<i64>`
  - `pub fn keys(&self) -> Result<Vec<String>>`
  - `pub fn clear(&self) -> Result<u64>`

- [ ] **Step 1: Write failing tests**

Append to `kv_store_tests.rs`:

```rust
#[test]
fn delete_exists_size_keys_clear() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    kv.set("b", "2")?;
    kv.set("a", "1")?;
    kv.set("c", "3")?;

    assert_eq!(kv.size()?, 3);
    assert!(kv.exists("a")?);
    assert!(!kv.exists("z")?);
    assert_eq!(kv.keys()?, vec!["a", "b", "c"]); // ORDER BY key ASC

    assert!(kv.delete("b")?);
    assert!(!kv.delete("b")?); // already gone
    assert_eq!(kv.size()?, 2);

    let removed = kv.clear()?;
    assert_eq!(removed, 2);
    assert_eq!(kv.size()?, 0);
    Ok(())
}

#[test]
fn list_stores_and_isolation() -> Result<()> {
    let tc = TestConnection::new()?;
    // Empty before any store has keys.
    assert!(tc.connection.kv_list_stores()?.is_empty());

    let a = tc.connection.kv_store("alpha")?;
    let b = tc.connection.kv_store("beta")?;
    a.set("k", "from_alpha")?;
    b.set("k", "from_beta")?; // same key, different store

    assert_eq!(a.get("k")?, Some("from_alpha".to_string()));
    assert_eq!(b.get("k")?, Some("from_beta".to_string()));

    let mut stores = tc.connection.kv_list_stores()?;
    stores.sort();
    assert_eq!(stores, vec!["alpha", "beta"]);
    Ok(())
}
```

- [ ] **Step 2: Run to verify failure**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests delete_exists_size_keys_clear`
Expected: FAIL — missing methods.

- [ ] **Step 3: Implement in `KvStore`'s impl block**

```rust
    /// Deletes `key`; returns `true` if a row was removed.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn delete(&self, key: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let affected = self.connection.command_params(
            &format!(
                "DELETE FROM {} WHERE store_name = $1 AND key = $2",
                self.table_ref
            ),
            &[&self.store_name.as_str(), &key],
        )?;
        Ok(affected > 0)
    }

    /// Returns whether `key` is present in this store.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn exists(&self, key: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let sql = format!(
            "SELECT 1 FROM {} WHERE store_name = $1 AND key = $2 LIMIT 1",
            self.table_ref
        );
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str(), &key])?
            .first_row()?
            .is_some())
    }

    /// Returns the number of keys in this store.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn size(&self) -> Result<i64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE store_name = $1",
            self.table_ref
        );
        // `scalar()` errors on zero rows, but COUNT(*) always returns exactly
        // one non-NULL row, so `unwrap_or(0)` is unreachable-but-defensive.
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])?
            .scalar::<i64>()?
            .unwrap_or(0))
    }

    /// Returns this store's keys, sorted ascending.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn keys(&self) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT key FROM {} WHERE store_name = $1 ORDER BY key ASC",
            self.table_ref
        );
        let mut result = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])?;
        let mut keys = Vec::new();
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                if let Some(k) = row.get::<String>(0) {
                    keys.push(k);
                }
            }
        }
        Ok(keys)
    }

    /// Deletes every key in this store; returns the number removed.
    ///
    /// The shared backing table survives; only this store's rows are removed.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn clear(&self) -> Result<u64> {
        self.connection.command_params(
            &format!("DELETE FROM {} WHERE store_name = $1", self.table_ref),
            &[&self.store_name.as_str()],
        )
    }
```

- [ ] **Step 4: Run the tests**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests`
Expected: PASS (all sync tests, including the new `size()`-based `delete_exists_size_keys_clear`).
No edit to the Task-3 test is needed — it deliberately never used `size()`.

- [ ] **Step 5: Clippy + fmt, then commit**

```bash
cargo clippy -p hyperdb-api --all-targets && cargo fmt -p hyperdb-api
git add hyperdb-api/src/kv_store.rs hyperdb-api/tests/kv_store_tests.rs
git commit -m "feat(kv): add delete/exists/size/keys/clear"
```

---

## Task 6: `pop` (transactional) + `set_batch` (transactional)

**Files:**
- Modify: `hyperdb-api/src/kv_store.rs`
- Modify: `hyperdb-api/tests/kv_store_tests.rs`

**Interfaces:**
- Consumes: `Connection::{begin_transaction_raw, commit_raw, rollback_raw}` (`pub(crate)`, take `&self`).
- Produces on `KvStore<'conn>`:
  - `pub fn pop(&self) -> Result<Option<(String, String)>>`
  - `pub fn set_batch(&self, entries: &[(&str, &str)]) -> Result<()>`

- [ ] **Step 1: Write failing tests**

Append to `kv_store_tests.rs`:

```rust
#[test]
fn pop_is_ordered_and_destructive() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("queue")?;
    kv.set("c", "3")?;
    kv.set("a", "1")?;
    kv.set("b", "2")?;

    assert_eq!(kv.pop()?, Some(("a".to_string(), "1".to_string())));
    assert_eq!(kv.pop()?, Some(("b".to_string(), "2".to_string())));
    assert_eq!(kv.pop()?, Some(("c".to_string(), "3".to_string())));
    assert_eq!(kv.pop()?, None); // empty
    assert_eq!(kv.size()?, 0);
    Ok(())
}

#[test]
fn set_batch_writes_all() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    kv.set_batch(&[("a", "1"), ("b", "2"), ("c", "3")])?;
    assert_eq!(kv.size()?, 3);
    assert_eq!(kv.get("b")?, Some("2".to_string()));
    // Batch upserts overwrite existing keys too.
    kv.set_batch(&[("b", "20"), ("d", "4")])?;
    assert_eq!(kv.get("b")?, Some("20".to_string()));
    assert_eq!(kv.size()?, 4);
    Ok(())
}

#[test]
fn set_batch_rejects_invalid_key_before_writing() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("cfg")?;
    let err = kv.set_batch(&[("ok", "1"), ("bad key", "2")]).unwrap_err();
    assert!(matches!(err, Error::InvalidName(_)));
    // Nothing was written because validation happens before the transaction.
    assert_eq!(kv.size()?, 0);
    Ok(())
}
```

- [ ] **Step 2: Run to verify failure**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests pop_is_ordered_and_destructive`
Expected: FAIL — missing `pop`/`set_batch`.

- [ ] **Step 3: Implement in `KvStore`'s impl block**

```rust
    /// Removes and returns the lowest-ordered key/value pair, or `None` if empty.
    ///
    /// The peek and delete run in one transaction, so they apply atomically —
    /// either both the read and the delete commit, or neither does (on error
    /// the transaction is rolled back). A SQL-NULL value is returned as an
    /// empty string.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn pop(&self) -> Result<Option<(String, String)>> {
        self.connection.begin_transaction_raw()?;
        let result = self.pop_inner();
        match &result {
            Ok(_) => self.connection.commit_raw()?,
            Err(_) => {
                // Best-effort rollback; preserve the original error.
                let _ = self.connection.rollback_raw();
            }
        }
        result
    }

    /// Transaction body for [`pop`](Self::pop).
    fn pop_inner(&self) -> Result<Option<(String, String)>> {
        let store = self.store_name.as_str();
        let select = format!(
            "SELECT key, value FROM {} WHERE store_name = $1 ORDER BY key ASC LIMIT 1",
            self.table_ref
        );
        // `first_row()` consumes the `Rowset`, dropping it (and releasing its
        // statement guard on the shared connection) BEFORE the DELETE runs —
        // the two statements never overlap on the connection.
        let Some(row) = self
            .connection
            .query_params(&select, &[&store])?
            .first_row()?
        else {
            return Ok(None);
        };
        let key: String = row
            .get::<String>(0)
            .ok_or_else(|| Error::internal("kv pop: key column was unexpectedly NULL"))?;
        let value: String = row.get::<String>(1).unwrap_or_default();
        self.connection.command_params(
            &format!(
                "DELETE FROM {} WHERE store_name = $1 AND key = $2",
                self.table_ref
            ),
            &[&store, &key.as_str()],
        )?;
        Ok(Some((key, value)))
    }

    /// Upserts every `(key, value)` pair in one transaction.
    ///
    /// All keys are validated before the transaction opens, so an invalid key
    /// aborts the whole batch without writing anything.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if any key is invalid (checked before writing).
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn set_batch(&self, entries: &[(&str, &str)]) -> Result<()> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw()?;
        let result = (|| {
            for (key, value) in entries {
                self.upsert(key, value)?;
            }
            Ok(())
        })();
        match &result {
            Ok(()) => self.connection.commit_raw()?,
            Err(_) => {
                let _ = self.connection.rollback_raw();
            }
        }
        result
    }
```

- [ ] **Step 4: Run the tests**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests`
Expected: PASS (all sync tests).

- [ ] **Step 5: Clippy + fmt, then commit**

```bash
cargo clippy -p hyperdb-api --all-targets && cargo fmt -p hyperdb-api
git add hyperdb-api/src/kv_store.rs hyperdb-api/tests/kv_store_tests.rs
git commit -m "feat(kv): add transactional pop and set_batch"
```

---

## Task 7: Compile-fail lifetime doc test

**Files:**
- Modify: `hyperdb-api/src/lib.rs`

**Interfaces:**
- Produces: a `compile_fail` doc test proving a `KvStore` cannot outlive its `Connection`, matching the existing `Inserter` example at `lib.rs:72-80`.

- [ ] **Step 1: Add the doc test**

In `lib.rs`, extend the `# Lifetime Safety` module doc. First add `KvStore` to the ASCII hierarchy list (after `Catalog<'conn>`):

```text
//! ├── Catalog<'conn>
//! ├── KvStore<'conn>
```

Then add a second `compile_fail` block after the existing `Inserter` one:

```rust
//! ```compile_fail
//! # use hyperdb_api::{Connection, CreateMode};
//! # fn example() -> hyperdb_api::Result<()> {
//! let conn = Connection::connect("localhost:7483", "test.hyper", CreateMode::CreateIfNotExists)?;
//! let kv = conn.kv_store("s")?;
//! drop(conn);  // ERROR: cannot move `conn` because it is borrowed by `kv`
//! let _ = kv.get("k")?;
//! # Ok(())
//! # }
//! ```
```

- [ ] **Step 2: Run the doc tests**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --doc`
Expected: PASS — the `compile_fail` block is expected to fail compilation (that is the assertion); all runnable/`no_run` doc examples compile.

- [ ] **Step 3: Commit**

```bash
cargo fmt -p hyperdb-api
git add hyperdb-api/src/lib.rs
git commit -m "test(kv): add compile-fail lifetime doc test for KvStore"
```

---

## Task 8: Async twin — `AsyncKvStore`

**Files:**
- Create: `hyperdb-api/src/async_kv_store.rs`
- Modify: `hyperdb-api/src/lib.rs` (`mod async_kv_store;` + `pub use async_kv_store::AsyncKvStore;`)
- Create: `hyperdb-api/tests/async_kv_store_tests.rs`

**Interfaces:**
- Consumes: `AsyncConnection::{execute_command, execute_query, query_params, command_params, begin_transaction_raw, commit_raw, rollback_raw}`; `AsyncRowset::{first_row, scalar, next_chunk}`; `kv_store::{KV_TABLE, validate_kv_name}`.
- Produces: `pub struct AsyncKvStore<'conn>` + `impl AsyncConnection { pub fn kv_store(&self, name) -> ...; pub async fn kv_list_stores(&self) -> ...; }` and all methods as `async fn`, mirroring Tasks 3-6.

> **Note:** `AsyncConnection::kv_store` runs a `CREATE TABLE`, which is `async`, so unlike the sync `kv_store` it must be `async fn kv_store(...) -> Result<AsyncKvStore<'_>>`. That is the only signature difference from the sync twin.

- [ ] **Step 1: Write failing async tests**

Create `hyperdb-api/tests/async_kv_store_tests.rs`:

```rust
// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for the async [`AsyncKvStore`] API.

mod common;

use common::{test_hyper_params, test_result_path};
use hyperdb_api::{AsyncConnection, CreateMode, Error, HyperProcess, Result};
use serde::{Deserialize, Serialize};

async fn fresh_async_conn(name: &str) -> Result<(HyperProcess, AsyncConnection)> {
    let db_path = test_result_path(name, "hyper")?;
    let params = test_hyper_params(name)?;
    let hyper = HyperProcess::new(None, Some(&params))?;
    let endpoint = hyper.require_endpoint()?.to_string();
    let conn = AsyncConnection::connect(
        &endpoint,
        db_path.to_str().expect("path"),
        CreateMode::CreateAndReplace,
    )
    .await?;
    Ok((hyper, conn))
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Profile {
    name: String,
    level: u32,
}

#[tokio::test(flavor = "current_thread")]
async fn async_kv_full_surface() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_full").await?;
    let kv = conn.kv_store("cfg").await?;

    assert_eq!(kv.get("missing").await?, None);
    kv.set("k", "v1").await?;
    kv.set("k", "v2").await?;
    assert_eq!(kv.get("k").await?, Some("v2".to_string()));

    let p = Profile { name: "ada".into(), level: 7 };
    kv.set_as("p", &p).await?;
    assert_eq!(kv.get_as::<Profile>("p").await?, Some(p));
    assert!(matches!(
        kv.get_as::<Profile>("k").await,
        Err(Error::Serialization(_))
    ));

    kv.set_batch(&[("a", "1"), ("b", "2")]).await?;
    assert_eq!(kv.size().await?, 4);
    assert_eq!(kv.keys().await?, vec!["a", "b", "k", "p"]);
    assert!(kv.exists("a").await?);
    assert!(kv.delete("a").await?);
    assert!(!kv.delete("a").await?);

    assert_eq!(kv.pop().await?, Some(("b".to_string(), "2".to_string())));

    let removed = kv.clear().await?;
    assert!(removed >= 1);
    assert_eq!(kv.size().await?, 0);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_list_stores_and_validation() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_list").await?;
    assert!(conn.kv_list_stores().await?.is_empty());
    conn.kv_store("alpha").await?.set("k", "1").await?;
    conn.kv_store("beta").await?.set("k", "2").await?;
    let mut stores = conn.kv_list_stores().await?;
    stores.sort();
    assert_eq!(stores, vec!["alpha", "beta"]);
    assert!(matches!(
        conn.kv_store("bad name").await,
        Err(Error::InvalidName(_))
    ));
    Ok(())
}
```

- [ ] **Step 2: Run to verify failure**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test async_kv_store_tests`
Expected: FAIL — `AsyncKvStore`/`kv_store` do not exist.

- [ ] **Step 3: Implement `async_kv_store.rs`**

Create `hyperdb-api/src/async_kv_store.rs`. Mirror `kv_store.rs` exactly, substituting `AsyncConnection`, `.await`, and `async fn`. Reuse the shared items from `kv_store`:

```rust
// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Async key-value store — the [`AsyncConnection`] twin of [`KvStore`](crate::KvStore).

use crate::async_connection::AsyncConnection;
use crate::error::{Error, Result};
use crate::kv_store::{kv_create_table_sql, validate_kv_name, KV_TABLE};

/// A handle to one named key-value store over an [`AsyncConnection`].
///
/// The async twin of [`KvStore`](crate::KvStore); see it for semantics. Open
/// one with [`AsyncConnection::kv_store`].
///
/// # Examples
///
/// ```no_run
/// use hyperdb_api::{AsyncConnection, CreateMode, Result};
///
/// async fn demo(conn: &AsyncConnection) -> Result<()> {
///     let kv = conn.kv_store("settings").await?;
///     kv.set("theme", "dark").await?;
///     assert_eq!(kv.get("theme").await?, Some("dark".to_string()));
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct AsyncKvStore<'conn> {
    connection: &'conn AsyncConnection,
    store_name: String,
    table_ref: String,
}

impl<'conn> AsyncKvStore<'conn> {
    async fn open(
        connection: &'conn AsyncConnection,
        name: &str,
        table_ref: String,
    ) -> Result<Self> {
        validate_kv_name(name, "store name")?;
        connection
            .execute_command(&kv_create_table_sql(&table_ref))
            .await?;
        Ok(AsyncKvStore {
            connection,
            store_name: name.to_string(),
            table_ref,
        })
    }

    pub(crate) async fn new(connection: &'conn AsyncConnection, name: &str) -> Result<Self> {
        Self::open(connection, name, KV_TABLE.to_string()).await
    }

    /// Async twin of [`KvStore::with_target`](crate::KvStore::with_target).
    ///
    /// `target` is interpolated into SQL — the caller must supply a
    /// pre-validated / identifier-escaped, SQL-safe qualifier (M2 must escape
    /// it before calling).
    #[allow(
        dead_code,
        reason = "M2 (hyperdb-mcp) consumer; kept here so M1 needs no later API change"
    )]
    pub(crate) async fn with_target(
        connection: &'conn AsyncConnection,
        name: &str,
        target: &str,
    ) -> Result<Self> {
        Self::open(connection, name, format!("{target}.{KV_TABLE}")).await
    }

    /// Returns this store's validated name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.store_name
    }

    /// Returns the value for `key`, or `None` if absent or NULL.
    ///
    /// # Errors
    ///
    /// See [`KvStore::get`](crate::KvStore::get).
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        validate_kv_name(key, "key")?;
        let sql = format!(
            "SELECT value FROM {} WHERE store_name = $1 AND key = $2",
            self.table_ref
        );
        let row = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str(), &key])
            .await?
            .first_row()
            .await?;
        Ok(row.and_then(|r| r.get::<String>(0)))
    }

    /// Sets `key` to `value` (upsert).
    ///
    /// # Errors
    ///
    /// See [`KvStore::set`](crate::KvStore::set).
    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        validate_kv_name(key, "key")?;
        self.upsert(key, value).await
    }

    async fn upsert(&self, key: &str, value: &str) -> Result<()> {
        let updated = self
            .connection
            .command_params(
                &format!(
                    "UPDATE {} SET value = $3 WHERE store_name = $1 AND key = $2",
                    self.table_ref
                ),
                &[&self.store_name.as_str(), &key, &value],
            )
            .await?;
        if updated == 0 {
            self.connection
                .command_params(
                    &format!(
                        "INSERT INTO {t} (store_name, key, value) \
                         SELECT $1, $2, $3 \
                         WHERE NOT EXISTS (SELECT 1 FROM {t} WHERE store_name = $4 AND key = $5)",
                        t = self.table_ref
                    ),
                    &[
                        &self.store_name.as_str(),
                        &key,
                        &value,
                        &self.store_name.as_str(),
                        &key,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    /// Deserializes the JSON value for `key` into `T`; `None` if absent.
    ///
    /// # Errors
    ///
    /// See [`KvStore::get_as`](crate::KvStore::get_as).
    pub async fn get_as<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key).await? {
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| Error::serialization(e.to_string())),
            None => Ok(None),
        }
    }

    /// Serializes `value` to JSON and stores it under `key` (upsert).
    ///
    /// # Errors
    ///
    /// See [`KvStore::set_as`](crate::KvStore::set_as).
    pub async fn set_as<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        validate_kv_name(key, "key")?;
        let json = serde_json::to_string(value).map_err(|e| Error::serialization(e.to_string()))?;
        self.upsert(key, &json).await
    }

    /// Deletes `key`; returns `true` if a row was removed.
    ///
    /// # Errors
    ///
    /// See [`KvStore::delete`](crate::KvStore::delete).
    pub async fn delete(&self, key: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let affected = self
            .connection
            .command_params(
                &format!(
                    "DELETE FROM {} WHERE store_name = $1 AND key = $2",
                    self.table_ref
                ),
                &[&self.store_name.as_str(), &key],
            )
            .await?;
        Ok(affected > 0)
    }

    /// Returns whether `key` is present.
    ///
    /// # Errors
    ///
    /// See [`KvStore::exists`](crate::KvStore::exists).
    pub async fn exists(&self, key: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let sql = format!(
            "SELECT 1 FROM {} WHERE store_name = $1 AND key = $2 LIMIT 1",
            self.table_ref
        );
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str(), &key])
            .await?
            .first_row()
            .await?
            .is_some())
    }

    /// Returns the number of keys in this store.
    ///
    /// # Errors
    ///
    /// See [`KvStore::size`](crate::KvStore::size).
    pub async fn size(&self) -> Result<i64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE store_name = $1",
            self.table_ref
        );
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])
            .await?
            .scalar::<i64>()
            .await?
            .unwrap_or(0))
    }

    /// Returns this store's keys, sorted ascending.
    ///
    /// # Errors
    ///
    /// See [`KvStore::keys`](crate::KvStore::keys).
    pub async fn keys(&self) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT key FROM {} WHERE store_name = $1 ORDER BY key ASC",
            self.table_ref
        );
        let mut result = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])
            .await?;
        let mut keys = Vec::new();
        while let Some(chunk) = result.next_chunk().await? {
            for row in &chunk {
                if let Some(k) = row.get::<String>(0) {
                    keys.push(k);
                }
            }
        }
        Ok(keys)
    }

    /// Deletes every key in this store; returns the number removed.
    ///
    /// # Errors
    ///
    /// See [`KvStore::clear`](crate::KvStore::clear).
    pub async fn clear(&self) -> Result<u64> {
        self.connection
            .command_params(
                &format!("DELETE FROM {} WHERE store_name = $1", self.table_ref),
                &[&self.store_name.as_str()],
            )
            .await
    }

    /// Removes and returns the lowest-ordered pair, or `None` if empty.
    ///
    /// # Errors
    ///
    /// See [`KvStore::pop`](crate::KvStore::pop).
    pub async fn pop(&self) -> Result<Option<(String, String)>> {
        self.connection.begin_transaction_raw().await?;
        let result = self.pop_inner().await;
        match &result {
            Ok(_) => self.connection.commit_raw().await?,
            Err(_) => {
                let _ = self.connection.rollback_raw().await;
            }
        }
        result
    }

    async fn pop_inner(&self) -> Result<Option<(String, String)>> {
        let select = format!(
            "SELECT key, value FROM {} WHERE store_name = $1 ORDER BY key ASC LIMIT 1",
            self.table_ref
        );
        let Some(row) = self
            .connection
            .query_params(&select, &[&self.store_name.as_str()])
            .await?
            .first_row()
            .await?
        else {
            return Ok(None);
        };
        let key: String = row
            .get::<String>(0)
            .ok_or_else(|| Error::internal("kv pop: key column was unexpectedly NULL"))?;
        let value: String = row.get::<String>(1).unwrap_or_default();
        self.connection
            .command_params(
                &format!(
                    "DELETE FROM {} WHERE store_name = $1 AND key = $2",
                    self.table_ref
                ),
                &[&self.store_name.as_str(), &key],
            )
            .await?;
        Ok(Some((key, value)))
    }

    /// Upserts every pair in one transaction; validates all keys first.
    ///
    /// # Errors
    ///
    /// See [`KvStore::set_batch`](crate::KvStore::set_batch).
    pub async fn set_batch(&self, entries: &[(&str, &str)]) -> Result<()> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw().await?;
        let mut inner: Result<()> = Ok(());
        for (key, value) in entries {
            if let Err(e) = self.upsert(key, value).await {
                inner = Err(e);
                break;
            }
        }
        match &inner {
            Ok(()) => self.connection.commit_raw().await?,
            Err(_) => {
                let _ = self.connection.rollback_raw().await;
            }
        }
        inner
    }
}

impl AsyncConnection {
    /// Opens a handle to a named KV store, creating the table if needed.
    ///
    /// # Errors
    ///
    /// See [`Connection::kv_store`](crate::Connection::kv_store).
    pub async fn kv_store(&self, name: &str) -> Result<AsyncKvStore<'_>> {
        AsyncKvStore::new(self, name).await
    }

    /// Lists the names of every KV store that currently holds at least one key.
    ///
    /// # Errors
    ///
    /// See [`Connection::kv_list_stores`](crate::Connection::kv_list_stores).
    pub async fn kv_list_stores(&self) -> Result<Vec<String>> {
        self.execute_command(&kv_create_table_sql(KV_TABLE)).await?;
        let mut result = self
            .execute_query(&format!(
                "SELECT DISTINCT store_name FROM {KV_TABLE} ORDER BY store_name ASC"
            ))
            .await?;
        let mut names = Vec::new();
        while let Some(chunk) = result.next_chunk().await? {
            for row in &chunk {
                if let Some(name) = row.get::<String>(0) {
                    names.push(name);
                }
            }
        }
        Ok(names)
    }
}
```

- [ ] **Step 4: Register the module and re-export**

In `lib.rs`, add `mod async_kv_store;` (near `mod async_connection;`) and `pub use async_kv_store::AsyncKvStore;` (near `pub use async_connection::AsyncConnection;`).

- [ ] **Step 5: Run the async tests**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test async_kv_store_tests -- --nocapture`
Expected: PASS (2 tests). If a test produces no output for ~30s, treat it as a hang/failure (repo rule #10), not a pass.

- [ ] **Step 6: Clippy + fmt, then commit**

```bash
cargo clippy -p hyperdb-api --all-targets && cargo fmt -p hyperdb-api
git add hyperdb-api/src/async_kv_store.rs hyperdb-api/src/lib.rs hyperdb-api/tests/async_kv_store_tests.rs
git commit -m "feat(kv): add AsyncKvStore async twin"
```

---

## Task 9: Performance benchmark — single-commit vs batched-commit

**Files:**
- Create: `hyperdb-api/benches/kv_benchmark.rs`
- Modify: `hyperdb-api/Cargo.toml` (register the example)

**Interfaces:**
- Consumes: `Connection::kv_store`, `KvStore::{clear, set, set_batch}`, `HyperProcess`, `Connection::new`. Standalone — does not use `benches/common.rs`.

**Design:** A plain-`main()` example (matching `benches/benchmark.rs`), run via `cargo run -p hyperdb-api --release --example kv_benchmark [N]`. It measures two write strategies for the KV store:
1. **Single-commit-per-set:** N calls to `kv.set(key, value)` (each an implicit upsert/commit).
2. **Batched:** N keys written in batches of `BATCH` (default 25, in the 10-50 range) via `kv.set_batch(&batch)`, one transaction per batch.

It reports rows/sec for each and the speedup factor.

- [ ] **Step 1: Create the benchmark**

Create `hyperdb-api/benches/kv_benchmark.rs`:

```rust
// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Key-value store write benchmark.
//!
//! Compares two write strategies against a real `hyperd`:
//! - single-commit-per-set: one `KvStore::set` per key (implicit commit)
//! - batched: `KvStore::set_batch` of `BATCH` keys per transaction
//!
//! Run with:
//!   cargo run -p hyperdb-api --release --example kv_benchmark            # default 50k keys
//!   cargo run -p hyperdb-api --release --example kv_benchmark 200000     # 200k keys

// Throughput math converts a `usize` key count to `f64`; the resulting
// precision loss is irrelevant for a keys/sec figure. `allow` (not `expect`)
// because this is the only pedantic cast lint that fires here — an `expect`
// listing others would trip `unfulfilled_lint_expectations` under `-D warnings`.
#![allow(
    clippy::cast_precision_loss,
    reason = "benchmark throughput math needs usize -> f64; precision loss is cosmetic"
)]

use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};
use std::env;
use std::time::Instant;

const DEFAULT_KEYS: usize = 50_000;
const BATCH: usize = 25; // within the requested 10-50 range

fn key_count() -> usize {
    env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_KEYS)
}

fn throughput(label: &str, keys: usize, secs: f64) {
    let per_sec = if secs > 0.0 { keys as f64 / secs } else { 0.0 };
    println!("  {label:<28} {keys} keys in {secs:>7.3}s  =>  {per_sec:>12.0} keys/sec");
}

fn bench_single(conn: &Connection, keys: usize) -> Result<f64> {
    let kv = conn.kv_store("bench_single")?;
    kv.clear()?;
    let start = Instant::now();
    for i in 0..keys {
        kv.set(&format!("k{i}"), "value")?;
    }
    Ok(start.elapsed().as_secs_f64())
}

fn bench_batched(conn: &Connection, keys: usize) -> Result<f64> {
    let kv = conn.kv_store("bench_batched")?;
    kv.clear()?;
    let start = Instant::now();
    let mut i = 0;
    while i < keys {
        let end = (i + BATCH).min(keys);
        // Own the strings, then borrow into the &[(&str, &str)] slice.
        let owned: Vec<(String, String)> =
            (i..end).map(|n| (format!("k{n}"), "value".to_string())).collect();
        let batch: Vec<(&str, &str)> =
            owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        kv.set_batch(&batch)?;
        i = end;
    }
    Ok(start.elapsed().as_secs_f64())
}

fn main() -> Result<()> {
    let keys = key_count();
    println!("\n=== KV Store write benchmark ({keys} keys, batch size {BATCH}) ===");

    let db_path = std::env::temp_dir().join("kv_benchmark.hyper");
    let hyper = HyperProcess::new(None, None)?;
    let conn = Connection::new(&hyper, &db_path, CreateMode::CreateAndReplace)?;

    let single_secs = bench_single(&conn, keys)?;
    throughput("single commit per set", keys, single_secs);

    let batched_secs = bench_batched(&conn, keys)?;
    throughput(&format!("batched ({BATCH}/txn)"), keys, batched_secs);

    if batched_secs > 0.0 {
        println!("\n  speedup (batched vs single): {:.2}x", single_secs / batched_secs);
    }
    Ok(())
}
```

> **Note:** Unlike the other benches, this one does **not** import `benches/common.rs` — its `ResourceMonitor` and other helpers are unused here, and an unused `mod common;` would trip `dead_code`/`unused` under `-D warnings`. Keep the file dependency-free and warning-clean. If you later want per-phase resource sampling, add `#[path = "common.rs"] mod common;` *and* actually call `ResourceMonitor` so nothing is dead.

- [ ] **Step 2: Register the example in `Cargo.toml`**

In `hyperdb-api/Cargo.toml`, in the benches-registered-as-examples block (after `benchmark_suite`), add:

```toml
[[example]]
name = "kv_benchmark"
path = "benches/kv_benchmark.rs"
```

- [ ] **Step 3: Build the benchmark (debug, to catch compile errors fast)**

Run: `cargo build -p hyperdb-api --example kv_benchmark`
Expected: clean build.

- [ ] **Step 4: Run a small smoke pass in release**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo run -p hyperdb-api --release --example kv_benchmark 2000`
Expected: prints both throughput lines and a speedup factor, with real (non-zero) numbers. Capture the output. If it hangs with no output ~30s, treat as failure (repo rule #10). Batched should be meaningfully faster than single-commit.

- [ ] **Step 5: Clippy + fmt, then commit**

```bash
cargo clippy -p hyperdb-api --all-targets && cargo fmt -p hyperdb-api
git add hyperdb-api/benches/kv_benchmark.rs hyperdb-api/Cargo.toml
git commit -m "feat(kv): add KV write benchmark (single vs batched commit)"
```

---

## Task 10: Documentation

**Files:**
- Modify: `hyperdb-api/README.md`
- Modify: `hyperdb-api/CHANGELOG.md`
- Modify: `hyperdb-api/DEVELOPMENT.md`

**Interfaces:** none (docs only). Rustdoc on every public item was written inline in Tasks 3-8; this task adds the crate-level surfaces and verifies doc warnings are clean.

- [ ] **Step 1: `CHANGELOG.md` — add an `### Added` bullet under `## [Unreleased]`**

```markdown
### Added

- Key-value store API: `Connection::kv_store` / `AsyncConnection::kv_store` returning
  `KvStore` / `AsyncKvStore` handles over a fixed `_hyperdb_kv_store` table, with
  `get`/`set`/`get_as`/`set_as`/`delete`/`exists`/`size`/`keys`/`pop`/`clear`/`set_batch`,
  plus `kv_list_stores`. Adds the `Error::Serialization` variant.
```

- [ ] **Step 2: `README.md` — overview entry + KV sub-section**

Add `KvStore` / `AsyncKvStore` to the "Key Types"/feature overview list, then add a two-level `## Key-Value Store` section after an existing feature section (e.g. after "Parameterized Queries"), with a realistic `no_run` example mirroring the rustdoc on `KvStore`. Keep implementation internals out of the README (they live in rustdoc / `DEVELOPMENT.md`, per `[[feedback_code_comments_over_docs]]`). Include the store-name/key validation rule and one `set_batch` example.

- [ ] **Step 3: `DEVELOPMENT.md` — add to "Features Implemented" + design note**

Add a "Key-Value Store" entry noting: single fixed backing table namespaced by `store_name`; upsert via UPDATE-then-conditional-INSERT (no `ON CONFLICT` in Hyper); `pop`/`set_batch` use `begin/commit/rollback_raw`; the `table_ref` seam reserved for the MCP milestone; and the empirically observed PK-enforcement behavior recorded in Task 3.

- [ ] **Step 4: Verify doc warnings are clean and doc tests pass**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p hyperdb-api --no-deps`
Expected: clean (no broken intra-doc links, no missing docs).

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --doc`
Expected: PASS (the `compile_fail` block asserts, `no_run` examples compile).

- [ ] **Step 5: Commit**

```bash
git add hyperdb-api/README.md hyperdb-api/CHANGELOG.md hyperdb-api/DEVELOPMENT.md
git commit -m "docs(kv): document KV store in README, CHANGELOG, DEVELOPMENT"
```

---

## Verification (run before Phase 5 sweep)

```bash
# Full API test suite (real hyperd), release-mode sanity, lint, docs.
make test-api
cargo clippy -p hyperdb-api --all-targets -- -D warnings
cargo fmt -p hyperdb-api --check
RUSTDOCFLAGS="-D warnings" cargo doc -p hyperdb-api --no-deps
HYPERD_PATH=~/dev/bin/hyperd cargo run -p hyperdb-api --release --example kv_benchmark 5000
```

Each command must produce real output and exit 0 (except the benchmark, which prints throughput). A silent hang is a failure, not a pass.

---

## Risks

- **PK enforcement unknown until probed** — Task 3 records it; the upsert guarantees correctness regardless. Public API unaffected.
- **`pop`/`set_batch` use `begin/commit/rollback_raw` on a shared `&self`** — this is the sanctioned escape hatch (`transaction()` needs `&mut self`, incompatible with the `&'conn Connection` borrow). Rollback on error is best-effort; the original error is preserved.
- **`DELETE`-based `clear` leaves MVCC tombstones** until compaction — negligible at KV scale; documented.
- **Benchmark string ownership** — `set_batch` takes `&[(&str, &str)]`; the benchmark materializes owned `String`s per batch then borrows. This adds allocation to the batched path but is identical per-key overhead, so the single-vs-batched comparison stays fair (both format keys the same way).

---

## Self-Review

**Spec coverage:** get/set/get_as/set_as (Task 4), delete/exists/size/keys/clear (Task 5), pop (Task 6), kv_list_stores (Tasks 3+5), name validation (Task 2), `Error::Serialization` (Task 1), sync+async twins (Tasks 3-6, 8), compile-fail lifetime test (Task 7), table-targeting seam (Task 3 `with_target`), transport gating (inherited from `query_params`/`command_params`, documented), docs (Task 10). **Added beyond spec:** `set_batch` (user request) + KV benchmark (user request, Task 9).

**Type consistency:** `size()` → `i64` (no cast); `delete()` → `bool`; `clear()`/`set_batch` return types match between sync (`Result<u64>`/`Result<()>`) and async. `validate_kv_name`/`KV_TABLE` shared from `kv_store` into `async_kv_store`. Method names identical across twins.

**Placeholder scan:** every code step is complete and compilable against verified signatures; no TBD/TODO. The only conditional is Task 3's temporary `size` stand-in, explicitly restored in Task 5 Step 4.

**Two spec corrections applied:** no "add serde deps" task (already deps); distinct-placeholder upsert (real extended-query protocol, not escaped literals).
