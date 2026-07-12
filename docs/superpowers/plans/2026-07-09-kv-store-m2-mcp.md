# KV Store — Milestone 2 (MCP tool surface) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the merged `hyperdb-api` KV store as MCP tools in `hyperdb-mcp`, usable against **any** database in the session (ephemeral primary, `persistent`, or any attached alias) via the standard optional `database` parameter, so an LLM can use it as a scratchpad wherever it wants.

**Architecture:** The MCP tools **reuse** the shipped `hyperdb-api` `KvStore` (not a reimplementation). M1 shipped an internal targeting seam (`KvStore::with_target`) as `pub(crate)`, unreachable from the separate `hyperdb-mcp` crate — so M2's first task is to finish that seam as a **public, safe-by-construction** API: `Connection::kv_store_in(database, name)` / `kv_list_stores_in(database)` (+ async twins). Crucially these take an **unescaped database name and escape it internally** (`escape_name`) into `"db"."public"._hyperdb_kv_store`, so the published surface has no raw-SQL-fragment footgun and the MCP passes bare aliases. This is not a breaking change: the KV API has never been published (it ships for the first time in v0.6.0 alongside M2). Each `kv_*` tool takes an optional `database` (and `persist`) parameter and resolves it through the server's existing `resolve_db` → `Engine::resolve_target_db` path (identical to `execute`/`query`/`load_*`): `None` ⇒ the ephemeral primary (plain `kv_store(name)`), `Some(alias)` ⇒ `kv_store_in(&alias, name)`. A KV store is just rows in a per-database `_hyperdb_kv_store` table, so it lives in — and is isolated per — whichever database it is opened against. Because `KvStore::open` always issues `CREATE TABLE IF NOT EXISTS`, KV is inherently a writable operation: **every** kv_* tool passes `require_writable: true` (so a read-only *attachment* is rejected cleanly), and the *mutating* tools additionally call the global `check_writable` gate (so a `--read-only` *server* blocks KV writes).

**Tech Stack:** Rust, `rmcp` 1.8 (`#[tool_router]` attribute macro), `hyperdb-api` (reused KV store), `serde`/`schemars` (tool params), real `hyperd` for tests.

## Global Constraints

- **PR title uses `feat:`** — M1 (retitled `chore:`) and M2 bundle into a single **v0.6.0** release. (Supersedes the design doc's stale `fix:` note — see Task 5, Step 6.)
- **Branch:** `feat/kv-mcp-tools` (already created off the merged `upstream/main` @ `7043b6f`).
- **Never invent `hyperd` flags/params.** Start servers only via `HyperProcess::new()` / the MCP's own bootstrap / `HYPERD_PATH=~/dev/bin/hyperd`. A silent hang is a failure, not a pass.
- **Never report a build/test as passing without seeing real output.** Check exit codes; ~30s of no output = failure.
- **No narrowing `as` casts on integers** (repo rule #7) — use `TryFrom` or a justified `#[expect(clippy::cast_*, reason = "...")]`.
- **Run `cargo clippy` + `cargo fmt` before every commit.** Match CI exactly: `HYPERD_PATH=~/dev/bin/hyperd cargo clippy --workspace --all-targets --all-features -- -D warnings`, on CI's `stable` toolchain (currently 1.97 — verify `rustup check`; local `stable` may lag).
- **`git add <files>` explicitly** (never `-A`). Conventional-commit messages.
- **Update `CHANGELOG.md`** for both `hyperdb-api` (the new public constructors) and `hyperdb-mcp` (the tools/resource) under `## [Unreleased]`.
- **Every new tool name** must be added to the hardcoded array in `hyperdb-mcp/tests/readme_tests.rs` **and** appear in `hyperdb-mcp/src/readme.rs`, or the structural coverage test fails. `readme_is_non_trivial` also caps the README at **< 20,000 bytes**; current README is ~11,800 bytes and this adds ~700, so there is ample headroom — but confirm with real test output (Task 5, Step 4).
- **DO NOT bump the workspace version, the `=`-pins, or `Cargo.lock`.** This repo uses **release-please**: version bumps (the shared `[workspace.package] version`), the `=`-pinned dependency updates in `hyperdb-mcp`/`hyperdb-compile-check`, and the `Cargo.lock` sync are all done by the bot in a `chore: release main` PR at release time. **Verified against M1 (#182, the sibling `feat:` branch for the same v0.6.0):** it touched neither the workspace version, the pins, nor `Cargo.lock` — it only added a `hyperdb-api/CHANGELOG.md` `### Added` bullet under `## [Unreleased]`. M2 mirrors that exactly. Contributors touch **only** the CHANGELOG.

---

## File Structure

- **`hyperdb-api/src/kv_store.rs`** — add public `Connection::kv_store_in` + `kv_list_stores_in` to the **existing `impl Connection` block (lines 419-466)**; add the private `kv_target_prefix` + `kv_list_stores_impl` helpers; remove `with_target`'s `#[allow(dead_code)]`. (The sync `Connection` KV methods live *here*, not in `connection.rs`.)
- **`hyperdb-api/src/async_kv_store.rs`** — add async `AsyncConnection::kv_store_in` + `kv_list_stores_in` to the **existing `impl AsyncConnection` block (lines 363-395)**; remove the async `with_target`'s `#[allow(dead_code)]`. (The async KV methods live *here*, not in `async_connection.rs`.)
- **`hyperdb-api/CHANGELOG.md`** — `### Added` bullet.
- **`hyperdb-api/tests/kv_store_in_tests.rs`** — new integration test (sync + async) for targeted KV location.
- **`hyperdb-mcp/src/server.rs`** — 8 `kv_*` tool handlers + param structs + a `kv_open` helper (opens the `KvStore` at the resolved database); register `hyper://schema/kv` resource (list + read dispatch).
- **`hyperdb-mcp/src/readme.rs`** — `### Key-value store` subsection under `## Tool index`, plus the LEFT JOIN enrichment note.
- **`hyperdb-mcp/tests/readme_tests.rs`** — add 8 tool names to the coverage array.
- **`hyperdb-mcp/tests/kv_tools_tests.rs`** — new integration test exercising the kv_* tools end-to-end.
- **`hyperdb-mcp/CHANGELOG.md`** — `### Added` bullet.
- **`docs/superpowers/specs/2026-07-08-kv-store-design.md`** — supersede stale PK / `fix:`-title wording (Task 5, Step 6).

---

## Interfaces (shipped M1 surface M2 consumes — verified against the files)

- **Sync KV methods live in `kv_store.rs`, NOT `connection.rs`:** `impl Connection { pub fn kv_store(&self, name: &str) -> Result<KvStore<'_>> }` — `kv_store.rs:437`; `pub fn kv_list_stores(&self) -> Result<Vec<String>>` — `kv_store.rs:451`. Same-module, so `KvStore`, `KV_TABLE`, `kv_create_table_sql`, `with_target` are reachable unqualified (no `crate::kv_store::` prefix).
- **Async KV methods live in `async_kv_store.rs`, NOT `async_connection.rs`:** `impl AsyncConnection { pub async fn kv_store(...) }` — `async_kv_store.rs:369`; `pub async fn kv_list_stores(...)` — `async_kv_store.rs:378`.
- `pub(crate) fn KvStore::with_target(connection, name, target) -> Result<Self>` builds `format!("{target}.{KV_TABLE}")` where `KV_TABLE = "_hyperdb_kv_store"` — `kv_store.rs:134-140`; async twin `async_kv_store.rs:66-72`. **`target` is interpolated raw** — the new public method is responsible for escaping.
- `pub(crate) fn kv_create_table_sql(table_ref: &str) -> String` — `kv_store.rs:70`; `pub(crate) const KV_TABLE` — `kv_store.rs:21`. Both re-used by the new location-aware list method.
- `KvStore` / `AsyncKvStore` methods (all `pub`): `get -> Result<Option<String>>`, `set(&str,&str) -> Result<()>`, `get_as<T: DeserializeOwned> -> Result<Option<T>>`, `set_as<T: Serialize> -> Result<()>`, `delete -> Result<bool>`, `exists -> Result<bool>`, `size -> Result<i64>`, `keys -> Result<Vec<String>>`, `clear -> Result<u64>`, `pop -> Result<Option<(String,String)>>`, `set_batch(&[(&str,&str)]) -> Result<()>`, `name -> &str`.
- `escape_name(&str) -> Result<String>` (quotes an identifier, 63-char limit) — `hyperdb-api/src/names.rs`, **publicly re-exported** (confirm `crate::escape_name` resolves from inside `kv_store.rs`; it is used by end users per the crate README).
- MCP `Engine::connection(&self) -> &Connection` — `engine.rs:683`. `impl From<hyperdb_api::Error> for McpError` exists (engine uses `.map_err(McpError::from)` throughout, incl. `ConnectionLost` mapping).
- **DB routing (the "tell it where" knob):** `Engine::resolve_target_db(Option<&str>) -> Result<String, McpError>` — `engine.rs:529` — maps `None`/`""`/`"persistent"`/`<alias>` to a concrete alias (lowercases aliases at `engine.rs:552`; errors on `"persistent"` under `--ephemeral-only`). The server wraps it: `HyperMcpServer::resolve_db(&self, engine, database: Option<&str>, persist: Option<bool>, require_writable: bool) -> Result<Option<String>, McpError>` — `server.rs:1024` — returns **`None` for the primary (leave SQL unqualified)** or **`Some(alias)` for a non-primary DB**, and enforces the read-only-*attachment* guard when `require_writable`. This is exactly how `execute`/`query`/`load_*` route their optional `database` param.
- MCP helpers: `check_writable(&str) -> Result<(), McpError>` (`server.rs:1005`, global `--read-only`-server gate), `with_engine(|&Engine| -> Result<R,McpError>) -> Result<R,McpError>` (`server.rs:1257`, holds the `self.engine` mutex + handles catalog bootstrap and `ConnectionLost` reconnect), `ok_content(Value)` (`server.rs:1422`), `err_content(McpError)` (`server.rs:1461`). Grep an existing data tool's param struct (e.g. `execute`/`query`) for the canonical `database`/`persist` doc-comment wording to copy.
- Tool handler shape: `fn NAME(&self, Parameters(p): Parameters<P>) -> Result<CallToolResult, rmcp::ErrorData>` inside the `#[tool_router] impl HyperMcpServer` block (`server.rs:1471`–`3378`). Tool name = fn name (no `name =` arg). Param structs are `#[derive(Debug, Deserialize, JsonSchema)]`, snake_case fields, each with a `///` doc comment.
- Resource wiring: capability already enabled (`server.rs:4042`). Add a `RawResource{...}.no_annotation()` entry to the static `vec!` in `list_resources` (`server.rs:4115`), an arm in `resource_body_for_uri` (`server.rs:3596`) returning `ResourceBody::Text { mime_type: "text/plain".into(), content }`, and (for parity) a URI in `list_resource_uris` (`server.rs:3727`).

---

## Task 1: Public, safe-by-construction KV targeting constructors in `hyperdb-api` (sync + async)

**Files:**
- Modify: `hyperdb-api/src/kv_store.rs` — add `kv_store_in` + `kv_list_stores_in` to the `impl Connection` block (~419-466); add `kv_target_prefix` + `kv_list_stores_impl` helpers; edit `with_target` doc + remove its `#[allow(dead_code)]` (~130-140)
- Modify: `hyperdb-api/src/async_kv_store.rs` — add async `kv_store_in` + `kv_list_stores_in` to the `impl AsyncConnection` block (~363-395); remove the async `with_target`'s `#[allow(dead_code)]` (~62-72)
- Modify: `hyperdb-api/CHANGELOG.md` (`### Added` bullet — **no Cargo.toml / version / lock changes**; release-please owns those, see Global Constraints)
- Test: `hyperdb-api/tests/kv_store_in_tests.rs` (new)

**Interfaces:**
- Produces:
  - `pub fn Connection::kv_store_in(&self, database: &str, name: &str) -> Result<KvStore<'_>>` + `pub async fn AsyncConnection::kv_store_in(&self, database: &str, name: &str) -> Result<AsyncKvStore<'_>>`.
  - `pub fn Connection::kv_list_stores_in(&self, database: &str) -> Result<Vec<String>>` + async twin.
  - `database` is an **unescaped** database (attachment) name; the method escapes it internally via `escape_name` into `"<database>"."public"._hyperdb_kv_store`. The public surface therefore carries no raw-SQL-fragment contract.
- Private helpers (in `kv_store.rs`):
  - `fn kv_target_prefix(database: &str) -> Result<String>` → escaped `"<database>"."public"` prefix. Single source of truth for the qualifier shape.
  - `fn Connection::kv_list_stores_impl(&self, table_ref: &str) -> Result<Vec<String>>` → the shared `create + SELECT DISTINCT` body, called by both `kv_list_stores` and `kv_list_stores_in` so they cannot drift.

### Design decisions (already reconciled — do not re-litigate)

1. **Safe-by-construction public API (was: raw `location: &str`).** `kv_store_in`/`kv_list_stores_in` take an *unescaped* `database` name and escape it internally. The publishable surface must not accept a pre-escaped SQL fragment — that would freeze a SQL-injection footgun into v0.6.0. Escaping lives entirely in `hyperdb-api`; the MCP passes bare aliases. `with_target(target)` stays the `pub(crate)` low-level escape hatch that still takes a pre-escaped qualifier.
2. **Schema is hard-wired to `public`.** Attached databases' tables live in their `public` schema in this engine (matches `saved_queries.rs` / `per_tool_database_tests.rs` 3-part `"db"."public"."table"` convention). A per-call schema param is YAGNI; document the `public` assumption.
3. **`kv_target_prefix` centralizes the qualifier** so `kv_store_in` (via `with_target`, which appends `.KV_TABLE`) and `kv_list_stores_in` (which appends `.KV_TABLE` itself) build byte-identical references.

- [ ] **Step 1: Write the failing test (sync + async)**

Create `hyperdb-api/tests/kv_store_in_tests.rs`:

```rust
// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Integration tests for `Connection::kv_store_in` / `kv_list_stores_in`
//! (targeted KV location), sync and async.

use hyperdb_api::{Connection, CreateMode, HyperProcess, Result};

/// A KV store opened in an attached database is isolated from the primary DB,
/// round-trips set/get through the attached location, and is enumerated by the
/// location-aware store listing.
#[test]
fn kv_store_in_targets_attached_database() -> Result<()> {
    let hyper = HyperProcess::new(None, None)?;

    // Primary (ephemeral) DB.
    let primary = std::env::temp_dir().join("kv_store_in_primary.hyper");
    let conn = Connection::new(&hyper, &primary, CreateMode::CreateAndReplace)?;

    // Attach a second DB under alias "aux".
    let aux_path = std::env::temp_dir().join("kv_store_in_aux.hyper");
    let _ = std::fs::remove_file(&aux_path);
    conn.execute_command(&format!(
        "CREATE DATABASE {}",
        hyperdb_api::escape_sql_path(&aux_path.to_string_lossy())
    ))?;
    conn.execute_command(&format!(
        "ATTACH DATABASE {} AS aux",
        hyperdb_api::escape_sql_path(&aux_path.to_string_lossy())
    ))?;

    // Open a KV store in the attached DB — pass the BARE alias; escaping is internal.
    let kv = conn.kv_store_in("aux", "settings")?;
    kv.set("theme", "dark")?;
    assert_eq!(kv.get("theme")?, Some("dark".to_string()));

    // The default-location store must NOT see the attached-DB value.
    let default_kv = conn.kv_store("settings")?;
    assert_eq!(default_kv.get("theme")?, None);

    // The location-aware listing sees the attached-DB store; the primary has none.
    assert_eq!(conn.kv_list_stores_in("aux")?, vec!["settings".to_string()]);
    assert!(conn.kv_list_stores()?.is_empty());

    Ok(())
}

/// Async twin of the above — proves the async `kv_store_in` / `kv_list_stores_in`
/// route to the attached DB and stay isolated from the primary.
#[tokio::test]
async fn async_kv_store_in_targets_attached_database() -> Result<()> {
    use hyperdb_api::AsyncConnection;

    let hyper = HyperProcess::new(None, None)?;
    let primary = std::env::temp_dir().join("async_kv_store_in_primary.hyper");
    let conn = AsyncConnection::new(&hyper, &primary, CreateMode::CreateAndReplace).await?;

    let aux_path = std::env::temp_dir().join("async_kv_store_in_aux.hyper");
    let _ = std::fs::remove_file(&aux_path);
    conn.execute_command(&format!(
        "CREATE DATABASE {}",
        hyperdb_api::escape_sql_path(&aux_path.to_string_lossy())
    ))
    .await?;
    conn.execute_command(&format!(
        "ATTACH DATABASE {} AS aux",
        hyperdb_api::escape_sql_path(&aux_path.to_string_lossy())
    ))
    .await?;

    let kv = conn.kv_store_in("aux", "settings").await?;
    kv.set("theme", "dark").await?;
    assert_eq!(kv.get("theme").await?, Some("dark".to_string()));
    assert_eq!(conn.kv_store("settings").await?.get("theme").await?, None);
    assert_eq!(conn.kv_list_stores_in("aux").await?, vec!["settings".to_string()]);

    Ok(())
}
```

> Confirm the exact async constructor (`AsyncConnection::new(&hyper, path, mode)` vs `connect`) against a sibling async test before running — mirror whatever the crate's existing async integration tests use. Confirm `escape_sql_path` is publicly re-exported (grep `pub use` in `hyperdb-api/src/lib.rs`); if not, build the `CREATE/ATTACH DATABASE` SQL the same way an existing test does.

- [ ] **Step 2: Run test to verify it fails**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_in_tests`
Expected: FAIL to compile — `no method named kv_store_in found for struct Connection`.

> If `CREATE DATABASE` / `ATTACH DATABASE` behave unexpectedly from a bare `Connection`, fall back to a same-DB **schema** target: `CREATE SCHEMA aux` + `conn.kv_store_in("aux", ...)` if `kv_store_in` targets `"aux"."public"` — or, if a schema (not DB) is needed, this is the signal that the API should key on a schema-qualified location instead. Probe with real output first (M1-style "probe the engine, then commit"); capture it before adjusting.

- [ ] **Step 3: Add the sync helpers + public methods**

In `hyperdb-api/src/kv_store.rs`, add the private prefix helper near `kv_create_table_sql` (top-of-file helper region):

```rust
/// Builds the escaped `"<database>"."public"` qualifier prefix for the KV table.
///
/// Single source of truth for the location shape used by
/// [`Connection::kv_store_in`] and [`Connection::kv_list_stores_in`] (and their
/// async twins): the KV table always lives in the target database's `public`
/// schema. `database` is escaped via [`escape_name`](crate::escape_name); the
/// fixed `public` schema name is escaped identically for symmetry.
pub(crate) fn kv_target_prefix(database: &str) -> Result<String> {
    Ok(format!(
        "{}.{}",
        crate::escape_name(database)?,
        crate::escape_name("public")?
    ))
}
```

Then in the **existing `impl Connection` block (`kv_store.rs:419-466`)**, refactor `kv_list_stores` to delegate to a shared body and add the two location-aware methods:

```rust
    /// Shared body for [`kv_list_stores`](Self::kv_list_stores) /
    /// [`kv_list_stores_in`](Self::kv_list_stores_in): create the backing table
    /// at `table_ref` (so a fresh location returns `[]`), then list the distinct
    /// store names there. Factored out so the default and location-aware paths
    /// cannot drift.
    fn kv_list_stores_impl(&self, table_ref: &str) -> Result<Vec<String>> {
        self.execute_command(&kv_create_table_sql(table_ref))?;
        let mut result = self.execute_query(&format!(
            "SELECT DISTINCT store_name FROM {table_ref} ORDER BY store_name ASC"
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

    /// Opens a handle to a KV store in a specific database, rather than the
    /// default (search-path) location.
    ///
    /// `database` is the **unescaped** name of an attached database; the store's
    /// backing table is created (if absent) in that database's `public` schema.
    /// The name is identifier-escaped internally, so it is safe to pass an
    /// arbitrary attachment alias. Store name, keys and values are always bound
    /// parameters.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use hyperdb_api::{Connection, CreateMode, Result};
    /// # fn example(conn: &Connection) -> Result<()> {
    /// let kv = conn.kv_store_in("persistent", "settings")?;
    /// kv.set("theme", "dark")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`](crate::Error::InvalidName) if `database` or `name` is invalid.
    /// - [`Error::FeatureNotSupported`](crate::Error::FeatureNotSupported) on gRPC transport.
    /// - [`Error::Server`](crate::Error::Server) if creating the backing table fails.
    pub fn kv_store_in(&self, database: &str, name: &str) -> Result<KvStore<'_>> {
        KvStore::with_target(self, name, &kv_target_prefix(database)?)
    }

    /// Lists the KV stores that hold at least one key in a specific database.
    ///
    /// The location-aware companion to [`kv_list_stores`](Self::kv_list_stores):
    /// `database` is the unescaped name of an attached database (escaped
    /// internally). Creates the backing table in that database's `public` schema
    /// first, so an empty database returns `[]` rather than erroring.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`](crate::Error::InvalidName) if `database` is invalid.
    /// - [`Error::FeatureNotSupported`](crate::Error::FeatureNotSupported) on gRPC transport.
    /// - [`Error::Server`](crate::Error::Server) if the query fails.
    pub fn kv_list_stores_in(&self, database: &str) -> Result<Vec<String>> {
        let table_ref = format!("{}.{KV_TABLE}", kv_target_prefix(database)?);
        self.kv_list_stores_impl(&table_ref)
    }
```

And change the existing `kv_list_stores` body (currently `kv_store.rs:452-464`) to:

```rust
    pub fn kv_list_stores(&self) -> Result<Vec<String>> {
        self.kv_list_stores_impl(KV_TABLE)
    }
```

> `escape_name` returns a *quoted* identifier (e.g. `"aux"`), so `kv_target_prefix("aux")` yields `"aux"."public"` and `kv_list_stores_in` builds `"aux"."public"._hyperdb_kv_store` — matching the engine's 3-part attached-table form. Verify `crate::escape_name` is reachable from `kv_store.rs`; if it is re-exported only at the crate root, `crate::escape_name` is correct.

- [ ] **Step 4: Update `with_target` doc + remove the `#[allow(dead_code)]` (sync)**

In `hyperdb-api/src/kv_store.rs:130-140`, delete the `#[allow(dead_code, reason = ...)]` block (the fn is now reached via `kv_store_in`) and retarget the doc comment to reference the public method:

```rust
    /// Opens a handle targeting an explicit, already-escaped table-qualifier prefix.
    ///
    /// Crate-internal low-level constructor behind
    /// [`Connection::kv_store_in`](crate::Connection::kv_store_in). `target` is
    /// interpolated directly into SQL, so the **caller must supply a
    /// pre-escaped, SQL-safe qualifier** — public callers go through
    /// `kv_store_in`, which escapes for them (`store_name` / `key` / `value` are
    /// always bound params, but `target` is not).
    pub(crate) fn with_target(
        connection: &'conn Connection,
        name: &str,
        target: &str,
    ) -> Result<Self> {
        Self::open(connection, name, format!("{target}.{KV_TABLE}"))
    }
```

- [ ] **Step 5: Add the async twins + remove their `#[allow(dead_code)]`**

In `hyperdb-api/src/async_kv_store.rs`:

- Extend the `use crate::kv_store::{...}` import (line 8) to also bring in `kv_target_prefix`.
- In the async `with_target` (`async_kv_store.rs:62-72`), delete the `#[allow(dead_code, reason = ...)]` block and retarget its doc to `AsyncConnection::kv_store_in` (mirror Step 4's wording).
- In the **existing `impl AsyncConnection` block (`async_kv_store.rs:363-395`)**, mirror Step 3: add a private `async fn kv_list_stores_impl(&self, table_ref: &str) -> Result<Vec<String>>` (async body — `.await` on `execute_command`/`execute_query`/`next_chunk`, as the current `kv_list_stores` does), rewrite `kv_list_stores` to call it with `KV_TABLE`, and add:

```rust
    /// Async twin of [`Connection::kv_store_in`](crate::Connection::kv_store_in).
    ///
    /// # Errors
    ///
    /// See [`Connection::kv_store_in`](crate::Connection::kv_store_in).
    pub async fn kv_store_in(&self, database: &str, name: &str) -> Result<AsyncKvStore<'_>> {
        AsyncKvStore::with_target(self, name, &kv_target_prefix(database)?).await
    }

    /// Async twin of [`Connection::kv_list_stores_in`](crate::Connection::kv_list_stores_in).
    ///
    /// # Errors
    ///
    /// See [`Connection::kv_list_stores_in`](crate::Connection::kv_list_stores_in).
    pub async fn kv_list_stores_in(&self, database: &str) -> Result<Vec<String>> {
        let table_ref = format!("{}.{KV_TABLE}", kv_target_prefix(database)?);
        self.kv_list_stores_impl(&table_ref).await
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_in_tests`
Expected: PASS (both sync and async cases). Capture real output.
Then: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api` — full API suite still green (M1's 918 baseline + new tests).

- [ ] **Step 7: CHANGELOG only (NO version bump)**

Add to `hyperdb-api/CHANGELOG.md` under `## [Unreleased]` → the existing `### Added` (append after the M1 KV bullet):

```text
- `Connection::kv_store_in(database, name)` / `kv_list_stores_in(database)` (plus the
  `AsyncConnection` twins) to open and enumerate KV stores in a specific attached
  database. The database name is identifier-escaped internally.
```

**Do NOT touch `hyperdb-api/Cargo.toml`, the `=`-pins, or `Cargo.lock`** — release-please owns the version bump + pin + lock sync (verified against M1 #182; see Global Constraints).

- [ ] **Step 8: Gate + commit**

Run: `cargo fmt -p hyperdb-api` then the full CI clippy command (Global Constraints). Confirm 0 warnings with real output. Also `RUSTDOCFLAGS="-D warnings" cargo doc -p hyperdb-api --no-deps` (the new rustdoc has intra-doc links).

```bash
git add hyperdb-api/src/kv_store.rs hyperdb-api/src/async_kv_store.rs \
        hyperdb-api/tests/kv_store_in_tests.rs hyperdb-api/CHANGELOG.md
git commit -m "feat(api): kv_store_in/kv_list_stores_in to target a KV store by database"
```

---

## Task 2: KV open helper on the MCP server

**Files:**
- Modify: `hyperdb-mcp/src/server.rs` (add one private helper near `resolve_db`)

**Interfaces:**
- Produces:
  - `fn kv_open<'e>(&self, engine: &'e Engine, database: Option<&str>, store: &str) -> Result<KvStore<'e>, McpError>` — opens the store at the resolved database (`kv_store` when `None` = primary, `kv_store_in(alias, store)` otherwise). Used by every store-scoped kv_* handler.

### Design decision (already reconciled)

Because `kv_store_in` now escapes internally, the MCP layer does **no** identifier escaping — it just forwards the alias from `resolve_db`. There is no `kv_location` helper. Each handler calls `resolve_db(engine, p.database.as_deref(), p.persist, true)` to get `Option<String>` (the target alias, or `None` for primary) and passes it straight to `kv_open` (or, for `kv_list_stores`, to a `match` over `kv_list_stores`/`kv_list_stores_in`). **Every** kv_* tool passes `require_writable: true` because opening any store issues `CREATE TABLE IF NOT EXISTS` — so KV requires a writable *target*; a read-only attachment is rejected cleanly up front. The *mutating* tools additionally call the global `check_writable` gate.

- [ ] **Step 1: Add the import**

Ensure `KvStore` is imported in `server.rs` (extend the existing `use hyperdb_api::{...}` with `KvStore`). `escape_name` is **not** needed in the MCP anymore. `Engine` is already in scope.

- [ ] **Step 2: Add the helper**

Inside `impl HyperMcpServer` (near `resolve_db`, `server.rs:1024`), add:

```rust
    /// Opens a KV store handle on the engine's connection, targeting the
    /// resolved `database` (`None` = the ephemeral primary's default location,
    /// `Some(alias)` = the persistent DB or an attached alias).
    ///
    /// `alias` comes straight from [`resolve_db`](Self::resolve_db); it is not
    /// escaped here — `Connection::kv_store_in` escapes the database name.
    fn kv_open<'e>(
        &self,
        engine: &'e Engine,
        database: Option<&str>,
        store: &str,
    ) -> Result<KvStore<'e>, McpError> {
        match database {
            Some(alias) => engine.connection().kv_store_in(alias, store),
            None => engine.connection().kv_store(store),
        }
        .map_err(McpError::from)
    }
```

- [ ] **Step 3: Compile-check (no handlers yet)**

Run: `cargo build -p hyperdb-mcp`
Expected: compiles, with a `dead_code` warning for the unused helper (acceptable transiently; Task 3 uses it). Do **not** add `#[allow]`; the next task consumes it. Capture output.

> No commit yet — Task 3 lands with this. (Task boundary is logical; commit at the end of Task 3.)

---

## Task 3: The eight `kv_*` tool handlers

**Files:**
- Modify: `hyperdb-mcp/src/server.rs` (param structs in the `// --- Parameter structs ---` region ~`server.rs:70-781`; handlers inside the `#[tool_router] impl` block near the saved-query neighborhood ~`server.rs:2842-2971`)

**Interfaces:**
- Produces MCP tools: `kv_get`, `kv_set`, `kv_delete`, `kv_list`, `kv_list_stores`, `kv_size`, `kv_pop`, `kv_clear`.

### Tool → KV method mapping

Every tool takes optional `database` + `persist` (routing). `db` below is `resolve_db(engine, p.database.as_deref(), p.persist, true)?` — always `require_writable: true`.

| Tool | Params | Resolve/open | Mutating? (calls `check_writable`) |
|---|---|---|---|
| `kv_get` | `{store, key, database?, persist?}` | `kv_open(engine, db.as_deref(), &store)` → `kv.get(&key)` | no |
| `kv_set` | `{store, key, value, database?, persist?}` | ` → kv.set(&key, &value)` | **yes** |
| `kv_delete` | `{store, key, database?, persist?}` | ` → kv.delete(&key)` | **yes** |
| `kv_list` | `{store, database?, persist?}` | ` → kv.keys()` | no |
| `kv_list_stores` | `{database?, persist?}` | `match db { Some(a) => kv_list_stores_in(a), None => kv_list_stores() }` | no |
| `kv_size` | `{store, database?, persist?}` | ` → kv.size()` | no |
| `kv_pop` | `{store, database?, persist?}` | ` → kv.pop()` | **yes** |
| `kv_clear` | `{store, database?, persist?}` | ` → kv.clear()` | **yes** |

- [ ] **Step 1: Add param structs**

In the parameter-structs region. **Copy the `database`/`persist` doc comments verbatim from an existing data tool's param struct** (grep the `execute`/`query` params) so the LLM-facing wording stays consistent:

```rust
/// Parameters for `kv_get` / `kv_delete`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvKeyParams {
    /// Namespace of the KV store (like a table of settings). Created on first write.
    pub store: String,
    /// Key to look up within the store.
    pub key: String,
    /// Which database holds the store: omit for the ephemeral primary,
    /// "persistent" for the durable DB, or any attached alias.
    /// (Replace with the EXACT wording copied from the existing `database` param.)
    #[serde(default)]
    pub database: Option<String>,
    /// Shortcut for database="persistent".
    /// (Replace with the EXACT wording copied from the existing `persist` param.)
    #[serde(default)]
    pub persist: Option<bool>,
}

/// Parameters for `kv_set`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvSetParams {
    /// Namespace of the KV store. Created on first write.
    pub store: String,
    /// Key to write. Overwrites any existing value for this key (upsert).
    pub key: String,
    /// Value to store. Any string, including a JSON document.
    pub value: String,
    /// Target database (see `database` on other tools).
    #[serde(default)]
    pub database: Option<String>,
    /// Shortcut for database="persistent".
    #[serde(default)]
    pub persist: Option<bool>,
}

/// Parameters for store-scoped tools (`kv_list`, `kv_size`, `kv_pop`, `kv_clear`).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvStoreParams {
    /// Namespace of the KV store to operate on.
    pub store: String,
    /// Target database (see `database` on other tools).
    #[serde(default)]
    pub database: Option<String>,
    /// Shortcut for database="persistent".
    #[serde(default)]
    pub persist: Option<bool>,
}

/// Parameters for `kv_list_stores` (no store name — lists all stores in a DB).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvListStoresParams {
    /// Target database (see `database` on other tools).
    #[serde(default)]
    pub database: Option<String>,
    /// Shortcut for database="persistent".
    #[serde(default)]
    pub persist: Option<bool>,
}
```

> Verify the exact field types/attrs the existing `database`/`persist` params use (they may already be `Option<String>` / `Option<bool>` with specific `#[schemars]`/`///` docs) and match them exactly — copy, don't paraphrase.

- [ ] **Step 2: Add the handlers** (inside the `#[tool_router] impl HyperMcpServer` block)

Each handler resolves the database inside `with_engine` (so `resolve_db`'s attach lookups run against the live engine — CONFIRMED deadlock-safe; see Risks), then opens the store there. **All** pass `require_writable: true`; mutators additionally call `check_writable` first. Note the **durability warning** baked into `kv_set`'s description (I1).

```rust
    #[tool(
        description = "Read a value from the KV scratchpad by store + key. \
                       Returns {found, value}; `value` is null when the key is absent. \
                       Omit `database` to read the ephemeral store; pass \
                       \"persistent\" (or persist=true) or an attached alias to read elsewhere."
    )]
    fn kv_get(&self, Parameters(p): Parameters<KvKeyParams>)
        -> Result<CallToolResult, rmcp::ErrorData>
    {
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = self.kv_open(engine, db.as_deref(), &p.store)?;
            kv.get(&p.key).map_err(McpError::from)
        });
        match result {
            Ok(value) => Self::ok_content(json!({ "found": value.is_some(), "value": value })),
            Err(e) => Self::err_content(e),
        }
    }

    #[tool(
        description = "KV scratchpad. Save a variable, state, summary, or JSON config \
                       under store + key to remember later without creating a database \
                       table. Overwrites any existing value (upsert). \
                       IMPORTANT: without `database` the value is written to the EPHEMERAL \
                       database and is LOST when the server restarts. To persist across \
                       restarts, pass database=\"persistent\" (or persist=true)."
    )]
    fn kv_set(&self, Parameters(p): Parameters<KvSetParams>)
        -> Result<CallToolResult, rmcp::ErrorData>
    {
        if let Err(e) = self.check_writable("kv_set") { return Self::err_content(e); }
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = self.kv_open(engine, db.as_deref(), &p.store)?;
            kv.set(&p.key, &p.value).map_err(McpError::from)
        });
        match result {
            Ok(()) => Self::ok_content(json!({ "stored": true, "store": p.store, "key": p.key })),
            Err(e) => Self::err_content(e),
        }
    }

    #[tool(
        description = "Delete a key from the scratchpad. Returns {deleted: true} when the \
                       key existed, {deleted: false} otherwise (no error)."
    )]
    fn kv_delete(&self, Parameters(p): Parameters<KvKeyParams>)
        -> Result<CallToolResult, rmcp::ErrorData>
    {
        if let Err(e) = self.check_writable("kv_delete") { return Self::err_content(e); }
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = self.kv_open(engine, db.as_deref(), &p.store)?;
            kv.delete(&p.key).map_err(McpError::from)
        });
        match result {
            Ok(deleted) => Self::ok_content(json!({ "deleted": deleted, "store": p.store, "key": p.key })),
            Err(e) => Self::err_content(e),
        }
    }

    #[tool(description = "List all keys in a scratchpad store, sorted ascending.")]
    fn kv_list(&self, Parameters(p): Parameters<KvStoreParams>)
        -> Result<CallToolResult, rmcp::ErrorData>
    {
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = self.kv_open(engine, db.as_deref(), &p.store)?;
            kv.keys().map_err(McpError::from)
        });
        match result {
            Ok(keys) => Self::ok_content(json!({ "store": p.store, "count": keys.len(), "keys": keys })),
            Err(e) => Self::err_content(e),
        }
    }

    #[tool(description = "List all scratchpad store namespaces that currently hold data in a database.")]
    fn kv_list_stores(&self, Parameters(p): Parameters<KvListStoresParams>)
        -> Result<CallToolResult, rmcp::ErrorData>
    {
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            match db.as_deref() {
                Some(alias) => engine.connection().kv_list_stores_in(alias),
                None => engine.connection().kv_list_stores(),
            }
            .map_err(McpError::from)
        });
        match result {
            Ok(stores) => Self::ok_content(json!({ "count": stores.len(), "stores": stores })),
            Err(e) => Self::err_content(e),
        }
    }

    #[tool(description = "Return the number of keys in a scratchpad store.")]
    fn kv_size(&self, Parameters(p): Parameters<KvStoreParams>)
        -> Result<CallToolResult, rmcp::ErrorData>
    {
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = self.kv_open(engine, db.as_deref(), &p.store)?;
            kv.size().map_err(McpError::from)
        });
        match result {
            Ok(size) => Self::ok_content(json!({ "store": p.store, "size": size })),
            Err(e) => Self::err_content(e),
        }
    }

    #[tool(
        description = "Destructively read-and-remove the lowest-keyed entry from a store \
                       (atomic peek+delete). Returns {found, key, value}."
    )]
    fn kv_pop(&self, Parameters(p): Parameters<KvStoreParams>)
        -> Result<CallToolResult, rmcp::ErrorData>
    {
        if let Err(e) = self.check_writable("kv_pop") { return Self::err_content(e); }
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = self.kv_open(engine, db.as_deref(), &p.store)?;
            kv.pop().map_err(McpError::from)
        });
        match result {
            Ok(Some((key, value))) =>
                Self::ok_content(json!({ "found": true, "key": key, "value": value })),
            Ok(None) => Self::ok_content(json!({ "found": false })),
            Err(e) => Self::err_content(e),
        }
    }

    #[tool(description = "Delete all keys in a scratchpad store. Returns the number removed.")]
    fn kv_clear(&self, Parameters(p): Parameters<KvStoreParams>)
        -> Result<CallToolResult, rmcp::ErrorData>
    {
        if let Err(e) = self.check_writable("kv_clear") { return Self::err_content(e); }
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = self.kv_open(engine, db.as_deref(), &p.store)?;
            kv.clear().map_err(McpError::from)
        });
        match result {
            Ok(removed) => Self::ok_content(json!({ "store": p.store, "removed": removed })),
            Err(e) => Self::err_content(e),
        }
    }
```

- [ ] **Step 3: Gate + build**

Run: `cargo build -p hyperdb-mcp` then `cargo fmt -p hyperdb-mcp`, then the full CI clippy command. 0 warnings, real output.

- [ ] **Step 4: Commit** (with Task 2)

```bash
git add hyperdb-mcp/src/server.rs
git commit -m "feat(mcp): add kv_* scratchpad tools backed by hyperdb-api KV store"
```

---

## Task 4: `hyper://schema/kv` MCP resource

**Files:**
- Modify: `hyperdb-mcp/src/server.rs` (`list_resources` static vec ~`4115`; `resource_body_for_uri` ~`3596`; `list_resource_uris` ~`3727`)

**Interfaces:**
- Produces: a static `text/plain` resource at `hyper://schema/kv` describing the KV backing-table schema, the ephemeral-vs-persistent durability rule, per-database isolation, and the LEFT JOIN enrichment pattern.

- [ ] **Step 1: Add the resource descriptor** to the static `vec![...]` in `list_resources` (`server.rs:4115`):

```rust
    RawResource {
        uri: "hyper://schema/kv".into(),
        name: "KV store schema".into(),
        title: Some("Key-value scratchpad schema".into()),
        description: Some(
            "Schema of the _hyperdb_kv_store table backing the kv_* tools, the \
             ephemeral-vs-persistent durability rule, and the LEFT JOIN enrichment \
             pattern for joining KV metadata onto analytical tables."
                .into(),
        ),
        mime_type: Some("text/plain".into()),
        size: None,
        icons: None,
        meta: None,
    }
    .no_annotation(),
```

- [ ] **Step 2: Add the dispatch arm** in `resource_body_for_uri` (`server.rs:3596`):

```rust
        if uri == "hyper://schema/kv" {
            return Ok(Some(ResourceBody::Text {
                mime_type: "text/plain".into(),
                content: KV_SCHEMA_RESOURCE.to_string(),
            }));
        }
```

Add the constant near other module consts in `server.rs`. **The schema text must reflect the SHIPPED table — NO composite PK** (the design doc's "composite PK" wording is stale; Hyper rejects it):

```rust
const KV_SCHEMA_RESOURCE: &str = "\
KV store backing table (managed by the kv_* tools):

  CREATE TABLE _hyperdb_kv_store (
      store_name TEXT NOT NULL,   -- namespace (the `store` tool argument)
      key        TEXT NOT NULL,   -- key within the store
      value      TEXT             -- value (nullable); may hold JSON
  );

There is NO PRIMARY KEY (Hyper disables indexes); (store_name, key) uniqueness is
enforced by the tool layer's upsert. Do not INSERT into this table directly — use
the kv_* tools, which guarantee uniqueness.

DATABASE / DURABILITY: each database has its own _hyperdb_kv_store table. Every
kv_* tool takes the same optional `database` parameter as the other tools. Omit it
and the store lives in the EPHEMERAL database — convenient, but LOST when the
server restarts. Pass \"persistent\" (or persist=true) to survive restarts, or any
attached alias to target that database. A store in one database is invisible from
another.

Enrich an analytical table with KV metadata without ALTER TABLE. The KV table must
be in the SAME database as the joined table (or fully qualify both) — a LEFT JOIN
cannot span databases implicitly:

  SELECT t.*, kv.value AS metadata
  FROM my_table t
  LEFT JOIN _hyperdb_kv_store kv
         ON t.id = kv.key AND kv.store_name = 'your_namespace'
  WHERE t.status = 'active';

ALWAYS include the `kv.store_name = '...'` filter: omitting it fans out any key
present in multiple stores (row multiplication).
";
```

- [ ] **Step 3: Add the URI to `list_resource_uris`** (`server.rs:3727`) alongside the other static URIs, for test parity.

- [ ] **Step 4: Gate + commit**

Run: `cargo build -p hyperdb-mcp`, `cargo fmt -p hyperdb-mcp`, full CI clippy. Real output, 0 warnings.

```bash
git add hyperdb-mcp/src/server.rs
git commit -m "feat(mcp): register hyper://schema/kv resource describing the KV table"
```

---

## Task 5: README + coverage test + LEFT JOIN doc + supersede stale spec

**Files:**
- Modify: `hyperdb-mcp/src/readme.rs` (`## Tool index` ~`readme.rs:95-168`)
- Modify: `hyperdb-mcp/tests/readme_tests.rs` (tools array ~`readme_tests.rs:32-57`)
- Modify: `docs/superpowers/specs/2026-07-08-kv-store-design.md` (supersede stale wording)

- [ ] **Step 1: Add the tool names to the coverage array** in `readme_tests.rs` (extend the `tools` array):

```rust
        "kv_get", "kv_set", "kv_delete", "kv_list",
        "kv_list_stores", "kv_size", "kv_pop", "kv_clear",
```

- [ ] **Step 2: Run the coverage test to verify it fails**

Run: `cargo test -p hyperdb-mcp --test readme_tests`
Expected: FAIL — `README missing mention of kv_get` (README not yet updated).

- [ ] **Step 3: Add the `### Key-value store` subsection** under `## Tool index` in `readme.rs`:

```
### Key-value store (scratchpad)

- `kv_set` — save a variable/state/summary/JSON under a store + key (upsert).
- `kv_get` — read a value by store + key.
- `kv_delete` — delete a key.
- `kv_list` — list keys in a store.
- `kv_list_stores` — list store namespaces that hold data in a database.
- `kv_size` — count keys in a store.
- `kv_pop` — destructively read-and-remove the lowest-keyed entry.
- `kv_clear` — delete all keys in a store.

Every kv_* tool takes the same optional `database` parameter as the data tools.
Omit it and the store lives in the EPHEMERAL database (lost on restart); pass
`"persistent"` (or `persist: true`) to persist across restarts, or any attached
alias to target that database. Each database has its own, isolated set of stores.

Enrich analytical tables with KV metadata via LEFT JOIN — always filter
`kv.store_name = '<namespace>'` to avoid row multiplication, and keep the KV table
in the same database as the joined table. See the `hyper://schema/kv` resource for
the join template.
```

- [ ] **Step 4: Run the coverage test to verify it passes**

Run: `cargo test -p hyperdb-mcp --test readme_tests`
Expected: PASS (all three README tests, incl. the `< 20,000` byte bound — confirm the printed byte count leaves headroom).

- [ ] **Step 5: Commit README**

```bash
git add hyperdb-mcp/src/readme.rs hyperdb-mcp/tests/readme_tests.rs
git commit -m "docs(mcp): document kv_* tools in the LLM-facing README"
```

- [ ] **Step 6: Supersede stale wording in the design spec** (I6)

The design doc (`docs/superpowers/specs/2026-07-08-kv-store-design.md`) predates two settled decisions: it describes a **composite PRIMARY KEY** (Hyper rejects it — the shipped table has no PK) and an M2 **`fix:`** PR title (now `feat:`). Add a short **`> **Superseded (2026-07-09):**`** note at the top of the doc (and inline at the PK/`fix:` mentions) pointing to this plan, rather than rewriting the historical spec wholesale. Then:

```bash
git add docs/superpowers/specs/2026-07-08-kv-store-design.md
git commit -m "docs: mark KV design spec's PK and fix:-title notes superseded"
```

---

## Task 6: End-to-end MCP integration test + CHANGELOG

**Files:**
- Test: `hyperdb-mcp/tests/kv_tools_tests.rs` (new) — mirror an existing MCP integration test's server construction (e.g. `attach_tests.rs` or a saved-query test).
- Modify: `hyperdb-mcp/CHANGELOG.md`

- [ ] **Step 1: Write the integration test**

Mirror an existing `hyperdb-mcp/tests/*` server-construction harness (find how they instantiate `HyperMcpServer` with a persistent workspace and how read-only / ephemeral-only servers are built). Call the tool handlers directly, as sibling tests do. Unless noted, use the default (ephemeral) database. Cover:
- `kv_set` then `kv_get` → `{found: true, value: "..."}`; `kv_get` on absent key → `{found: false, value: null}`.
- `kv_set` overwrite → `kv_get` returns the new value.
- `kv_list` sorted; `kv_size` count; `kv_list_stores` includes the store.
- `kv_delete` → `{deleted: true}`, then again → `{deleted: false}`.
- `kv_pop` returns lowest key and removes it; on empty store → `{found: false}`.
- `kv_clear` returns removed count; store then empty.
- **Database routing (the headline behavior):** with a persistent workspace, `kv_set` with `database: Some("persistent")` (or `persist: Some(true)`) then `kv_get` on the **default** database for the same store/key → `{found: false}` (isolation). And `kv_list_stores` with `database: Some("persistent")` lists the persistent store while the default-DB `kv_list_stores` does not (proves `kv_list_stores_in` routing).
- **Attached-DB routing (if a sibling test shows how to `attach_database` in-process):** `kv_set` into an attached alias, read it back via the same alias, confirm the default DB doesn't see it. If wiring an attach in the MCP harness is heavy, this is already covered at the API level (Task 1) — note that here rather than duplicating.
- **`--ephemeral-only` guard:** on an ephemeral-only server, `kv_set` with `database: Some("persistent")` returns an error content (the `resolve_target_db` "no persistent database" error), not a panic.
- **Read-only server mode:** construct a `--read-only` server. Mutators `kv_set`/`kv_delete`/`kv_pop`/`kv_clear` return a read-only error content (blocked by `check_writable`). **Readers** `kv_get`/`kv_list`/`kv_size` are NOT gated by `check_writable` — assert their behavior and **capture real output** to settle the create-on-open question: opening a store issues `CREATE TABLE IF NOT EXISTS`, so verify whether a reader succeeds against a writable target in a read-only *server*. If the engine rejects the no-op create (table already present) or the connection forbids DDL in read-only mode, record it — the mitigation is to gate readers with `check_writable` too (a one-line change per reader), which we then apply and re-run. Do not assume; assert on captured output.
- **Persistence:** in a persistent workspace, a value written with `database: "persistent"` survives dropping and rebuilding the server against the same workspace path (mirror how saved-query persistence tests assert this, if present).

> Use `HYPERD_PATH=~/dev/bin/hyperd`. Capture real output; a hang = failure.

- [ ] **Step 2: Run the integration test**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests`
Expected: PASS, all cases. Real output.

- [ ] **Step 3: CHANGELOG**

`hyperdb-mcp/CHANGELOG.md` under `## [Unreleased]` → `### Added`:
```
- `kv_get`, `kv_set`, `kv_delete`, `kv_list`, `kv_list_stores`, `kv_size`, `kv_pop`, `kv_clear` tools — a key-value scratchpad backed by the `hyperdb-api` KV store, routable to any database via the standard `database`/`persist` parameters.
- `hyper://schema/kv` resource describing the KV table schema, durability rule, and LEFT JOIN enrichment pattern.
```

- [ ] **Step 4: Gate + commit**

Run `cargo fmt` + full CI clippy. Real output.

```bash
git add hyperdb-mcp/tests/kv_tools_tests.rs hyperdb-mcp/CHANGELOG.md
git commit -m "test(mcp): end-to-end kv_* tool coverage incl. routing, read-only, persistence"
```

---

## Verification Commands (Phase 5 — full E2E)

```bash
# 1. Format + lint, CI-exact (verify toolchain matches CI's stable first via `rustup check`)
cargo fmt --all --check
HYPERD_PATH=~/dev/bin/hyperd cargo clippy --workspace --all-targets --all-features -- -D warnings

# 2. Docs clean
RUSTDOCFLAGS="-D warnings" cargo doc -p hyperdb-api -p hyperdb-mcp --no-deps

# 3. Targeted suites
HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_in_tests
HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests
HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test readme_tests

# 4. Full workspace
HYPERD_PATH=~/dev/bin/hyperd cargo test --workspace

# 5. Real-tooling smoke: launch the MCP, confirm kv_* tools appear in tools/list and get_readme,
#    and that hyper://schema/kv is listed + readable (mirror how existing smoke checks run).
```

All must pass with **captured real output** before shipping.

---

## Risks

- **`resolve_db` inside `with_engine` — CONFIRMED safe.** Each handler calls `self.resolve_db(engine, ...)` from within the `with_engine` closure. Verified: `with_engine` holds the `self.engine` mutex (`server.rs:1271` warns against re-locking it), but `resolve_db` (`server.rs:1024`) operates on the `&Engine` passed in and reads `self.attachments` — it never locks `self.engine`. No re-entrancy/deadlock. Keep the `resolve_db` call inside the closure so attach lookups see live engine state.
- **Create-on-open vs read-only server (I2) — verify empirically.** `KvStore::open` unconditionally issues `CREATE TABLE IF NOT EXISTS` (`kv_store.rs:110`), so every kv_* tool does DDL on open. `require_writable: true` everywhere gives a clean error for read-only *attachments*. Readers are intentionally NOT gated by the global `check_writable`, so a reader can work in a `--read-only` *server* against a writable target — but only if the no-op create is allowed there. Task 6 Step 1 asserts this on captured output; if the engine rejects it, the fix (gate readers with `check_writable` too) is one line per reader.
- **Public API is a one-way door.** `kv_store_in(database, name)` publishes in v0.6.0. The safe-by-construction shape (unescaped name, escaped internally; `public` schema) was chosen precisely because the MCP validates the *real* API and a raw-fragment signature would be an un-removable footgun. If a schema-qualified variant is ever needed, add a new method — do not change this signature.
- **`kv_list_stores` targeting.** Resolved: Task 1 adds `Connection::kv_list_stores_in(database)`; the handler routes `Some(alias) => kv_list_stores_in(alias)`, `None => kv_list_stores()`. The two list-stores bodies share `kv_list_stores_impl` so they cannot drift.
- **`CREATE/ATTACH DATABASE` from a bare `Connection`.** Task 1's test assumes it works (the MCP engine does it). Mitigation: Step 2 fallback to a schema-qualified location if attach misbehaves — probe with real output first (M1-style).
- **`escape_name(alias)` matches the ATTACH form.** `resolve_target_db` **lowercases** aliases (`engine.rs:552`) so `"alias"."public"."t"` matches the attached table. Feeding that lowercased alias through `escape_name` inside `kv_store_in` yields `"alias"."public"._hyperdb_kv_store` — verify it matches `saved_queries.rs::qualified_table`'s shape.
- **Ephemeral mode starts `hyperd`.** Unlike saved-queries' in-memory `SessionStore`, KV is table-backed so any kv_* call spins up the engine. Accepted (documented) — a pure-memory reimplementation was rejected as it wouldn't validate the real API.
- **`hyperdb-api` version-pin lockstep is release-please's job, NOT this PR's.** The `=`-pins in `hyperdb-mcp`/`hyperdb-compile-check` and the shared workspace version are bumped by the bot in a `chore: release main` PR at release time. Verified against M1 (#182): the feature branch left all of them untouched and only added a CHANGELOG bullet. M2 must do the same — touching the version here would diverge from M1 and collide with release-please.

---

## Notes carried from Phase 0 + reconciled review (do not re-litigate)

- **Reuse the `hyperdb-api` KV store** — the user explicitly wants the MCP to validate the real API; making the API change here is by design.
- **Safe-by-construction public API** — the published `kv_store_in(database, name)` takes an *unescaped* database name and escapes internally. The MCP does no escaping. (Reviewer C1.)
- **Sync KV `impl Connection` lives in `kv_store.rs`; async `impl AsyncConnection` in `async_kv_store.rs`** — NOT `connection.rs`/`async_connection.rs`. (Both reviewers.)
- **Route KV through the existing `database`/`persist` params** — a KV store can live in any database; the MCP already has `resolve_db` → `resolve_target_db`. User-confirmed model.
- **Ephemeral by default** (matches `execute`/`query`/`load`), with a loud durability warning in `kv_set`'s description and the `hyper://schema/kv` resource. (Reviewer I1; user decision 2026-07-09.)
- **Every kv_* tool passes `require_writable: true`; mutators also call `check_writable`.** (Reviewer I2.)
- No Session/Workspace store split (KV is table-backed; the in-memory optimization does not apply).
- Tool registration is automatic via `#[tool(...)]` inside the `#[tool_router] impl` block — coverage enforced only by `readme_tests.rs` (substring check): both the array and the README must list every kv_* name.
