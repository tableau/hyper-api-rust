# KV Store — LLM-First MCP Ergonomics (Issue #192) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the HyperDB KV MCP tools materially easier for an LLM to drive — insert/overwrite signal + write guard, server-side `value_path`, byte-size reporting, batch write/read, and JSON-query/error docs — without silent data loss.

**Architecture:** Thread a `created` bit (already computed and discarded in `upsert`) up through new `SetOutcome`/`BatchSetOutcome` types in both the sync `KvStore` and async `AsyncKvStore` twins; add `set_if_absent`, `byte_size`, and an atomic batch-if-absent variant in the API crate; then expose the new signals and two new tools (`kv_set_many`, `values` flag on `kv_list`) plus improved error fidelity and docs in the `hyperdb-mcp` crate. The breaking API change is carried by a `feat!:` commit so release-please performs the `0.6.1 → 0.7.0` minor bump.

**Tech Stack:** Rust (workspace), `hyperdb-api` (sync + async KV over the shared `_hyperdb_kv_store` TEXT table, no PK, no `ON CONFLICT`), `hyperdb-mcp` (rmcp 1.x `#[tool_router]`), real `hyperd` subprocess for integration tests, release-please (`release-type: simple`, `bump-minor-pre-major`).

## Global Constraints

Every task's requirements implicitly include this section. Values copied verbatim from the spec and project rules.

- **Base branch:** `main` @ `d2ab4fc` (== `origin/main` == `upstream/main` == tag `v0.6.1`; KV work #182/#185/#188/#189 all merged).
- **Test invocation:** all tests run under `HYPERD_PATH=~/dev/bin/hyperd` (real `hyperd` subprocess). Never invent `hyperd` flags; start servers only via `HyperProcess::new()` / Makefile / `HYPERD_PATH`.
- **CI lint gate (exact):** `cargo clippy --workspace --all-targets --all-features -- -D warnings` on CI's `stable`, plus `cargo fmt --all --check`. The workspace pins `pedantic = "warn"`, which under `-D warnings` is effectively DENY. Consequences: every new `pub fn` needs `# Errors` rustdoc, every new struct needs `#[derive(Debug)]`, and NO float casts (report raw byte counts, never computed MiB — avoids `cast_precision_loss`).
- **No narrowing `as` casts on integers** (AGENTS.md rule 7): use `TryFrom`. `value.len()` is `usize`; report it as-is or via `i64::try_from(...).unwrap_or(i64::MAX)` where an `i64` is needed — never `as i64`.
- **Sync/async twin lockstep:** every KV change lands in BOTH `kv_store.rs` and `async_kv_store.rs`. The async `set_batch` uses a different loop shape (`let mut inner; for … { if let Err(e) = …await { inner = Err(e); break; } }`) than the sync closure form — preserve each file's existing pattern.
- **Changelog:** append per-crate `## [Unreleased]` bullets ONLY. Do NOT hand-edit `Cargo.toml` versions, the workspace version, the root `CHANGELOG.md`, or `.release-please-manifest.json` — release-please owns all of those. The `0.6.1 → 0.7.0` bump is carried by a `feat!:` commit / `BREAKING CHANGE:` footer.
- **Never report a build/test green without captured output** (AGENTS.md rule 10); check exit codes; ~30s of no output = hanging/failed.
- **Run `cargo clippy` + `cargo fmt` before every commit.**
- **Update `hyperdb-mcp/src/readme.rs`** whenever the MCP tool surface changes (adding `kv_set_many`), and update the `readme_tests.rs` tool-name list to match.

---

## File Structure

| File | Responsibility | Tasks |
|---|---|---|
| `hyperdb-api/src/kv_store.rs` | Sync `KvStore`: `SetOutcome`/`BatchSetOutcome`, reshaped `set`/`set_as`/`set_batch`/`upsert`, new `set_if_absent`/`byte_size`/`entries`/`set_batch_if_absent` | 1, 3, 4, 5 |
| `hyperdb-api/src/async_kv_store.rs` | Async twins of every `kv_store.rs` change | 2, 6 |
| `hyperdb-api/src/lib.rs` | Re-export `SetOutcome`, `BatchSetOutcome` | 1 |
| `hyperdb-api/tests/kv_store_tests.rs` | Sync integration tests (real `hyperd`) | 1, 3, 4, 5 |
| `hyperdb-api/tests/async_kv_store_tests.rs` | Async integration tests | 2, 6 |
| `hyperdb-api/CHANGELOG.md` | `## [Unreleased]` bullets (Changed BREAKING + Added) | 13 |
| `hyperdb-mcp/src/error.rs` | `PermissionDenied` mapping + fix misleading 0A000/JSON suggestion | 7 |
| `hyperdb-mcp/src/server.rs` | `KvSetParams`/`KvListParams`/`KvSetManyParams`/`KvEntry`; `kv_set`/`kv_size`/`kv_set_many`/`kv_list` handlers; `value_path`; size warning + `value_bytes` | 8, 9, 10, 11 |
| `hyperdb-mcp/src/readme.rs` | JSON-query + `::numeric` docs, `kv_set_many`, `value_path`/`overwrite`/`values` | 12 |
| `hyperdb-mcp/tests/kv_tools_tests.rs` | MCP tool coverage via `TestHarness` | 8, 9, 10, 11 |
| `hyperdb-mcp/tests/readme_tests.rs` | Add `kv_set_many` to the asserted tool-name list | 12 |
| `hyperdb-mcp/CHANGELOG.md` | `## [Unreleased]` Added/Fixed bullets | 13 |

**Task map:** 1 sync outcome · 2 async outcome · 3 sync `set_if_absent` · 4 sync `byte_size`+`entries` · 5 sync `set_batch_if_absent` · 6 async twins (3/4/5) · 7 error.rs fixes · 8 MCP `kv_set` overhaul · 9 MCP `kv_size` bytes · 10 MCP `kv_set_many` · 11 MCP `kv_list values` · 12 readme+readme_tests · 13 CHANGELOGs.

---

### Task 1: `SetOutcome`/`BatchSetOutcome` + reshape sync `set`/`set_as`/`set_batch`/`upsert`

The `created` bit is already computed in `upsert` (`updated == 0`) and discarded. Introduce the two outcome types, change `upsert` to return `Result<bool>` (`created`), and thread it up. This is the BREAKING change (`Result<()>` → `Result<SetOutcome>`).

**Files:**
- Modify: `hyperdb-api/src/kv_store.rs:184-222` (`set`, `upsert`), `:242-253` (`set_as`), `:403-430` (`set_batch`); add the two structs near the top of the module.
- Modify: `hyperdb-api/src/lib.rs:217` (re-export)
- Test: `hyperdb-api/tests/kv_store_tests.rs`

**Interfaces:**
- Produces: `pub struct SetOutcome { pub created: bool }`; `pub struct BatchSetOutcome { pub created: usize, pub overwritten: usize }`; `KvStore::set(&self, &str, &str) -> Result<SetOutcome>`; `KvStore::set_as<T: Serialize>(&self, &str, &T) -> Result<SetOutcome>`; `KvStore::set_batch(&self, &[(&str,&str)]) -> Result<BatchSetOutcome>`; private `KvStore::upsert(&self, &str, &str) -> Result<bool>`.
- Consumed by: Task 2 (MCP `kv_set`), Task 7 (`kv_set_many`), Task 6 (async twin mirrors these names).

- [ ] **Step 1: Write the failing test**

Add to `hyperdb-api/tests/kv_store_tests.rs`:

```rust
#[test]
fn set_reports_created_then_overwritten() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("outcome")?;
    let first = kv.set("k", "v1")?;
    assert!(first.created, "first write of a key must report created=true");
    let second = kv.set("k", "v2")?;
    assert!(!second.created, "overwrite must report created=false");
    assert_eq!(kv.get("k")?, Some("v2".to_string()));
    Ok(())
}

#[test]
fn set_batch_reports_created_and_overwritten() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("batch_outcome")?;
    kv.set("a", "1")?; // pre-existing → will be overwritten
    let out = kv.set_batch(&[("a", "10"), ("b", "20"), ("c", "30")])?;
    assert_eq!(out.created, 2, "b and c are new");
    assert_eq!(out.overwritten, 1, "a existed");
    assert_eq!(kv.get("a")?, Some("10".to_string()));
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests set_reports_created`
Expected: FAIL — `no method named created` / `set_batch` returns `()` not `BatchSetOutcome`.

- [ ] **Step 3: Add the outcome types**

Near the top of `hyperdb-api/src/kv_store.rs` (after the existing `use` lines, before `struct KvStore`), add:

```rust
/// Outcome of a single KV write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetOutcome {
    /// `true` if the key did not previously exist (insert); `false` if an
    /// existing value was overwritten.
    pub created: bool,
}

/// Outcome of a batch KV write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchSetOutcome {
    /// Number of keys newly inserted.
    pub created: usize,
    /// Number of keys whose prior value was replaced.
    pub overwritten: usize,
}
```

- [ ] **Step 4: Reshape `upsert` to return `created`**

Replace the body tail of `upsert` (`kv_store.rs:201-222`). Change the return type and return `updated == 0`:

```rust
    /// UPDATE-then-conditional-INSERT upsert. Assumes `key` is validated.
    /// Returns `true` if the row was newly inserted (created), `false` if an
    /// existing value was overwritten.
    ///
    /// Hyper has no `ON CONFLICT`; this mirrors the proven `_table_catalog`
    /// idiom. The conditional INSERT uses distinct placeholders (`$4`/`$5`)
    /// so it is unambiguous under the extended-query protocol.
    fn upsert(&self, key: &str, value: &str) -> Result<bool> {
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
        Ok(updated == 0)
    }
```

- [ ] **Step 5: Reshape `set`, `set_as`, `set_batch`**

`set` (`kv_store.rs:191-194`):

```rust
    pub fn set(&self, key: &str, value: &str) -> Result<SetOutcome> {
        validate_kv_name(key, "key")?;
        Ok(SetOutcome { created: self.upsert(key, value)? })
    }
```

`set_as` (`kv_store.rs:249-253`):

```rust
    pub fn set_as<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<SetOutcome> {
        validate_kv_name(key, "key")?;
        let json = serde_json::to_string(value).map_err(|e| Error::serialization(e.to_string()))?;
        Ok(SetOutcome { created: self.upsert(key, &json)? })
    }
```

`set_batch` (`kv_store.rs:412-430`) — accumulate per-entry bool into the counts; keep the closure form:

```rust
    pub fn set_batch(&self, entries: &[(&str, &str)]) -> Result<BatchSetOutcome> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw()?;
        let result = (|| {
            let mut outcome = BatchSetOutcome { created: 0, overwritten: 0 };
            for (key, value) in entries {
                if self.upsert(key, value)? {
                    outcome.created += 1;
                } else {
                    outcome.overwritten += 1;
                }
            }
            Ok(outcome)
        })();
        match &result {
            Ok(_) => self.connection.commit_raw()?,
            Err(_) => {
                let _ = self.connection.rollback_raw();
            }
        }
        result
    }
```

Also update the `set_batch` rustdoc `# Errors` block to keep the existing bullets (unchanged text) and the `set`/`set_as` doc comments' first line to mention the returned outcome.

- [ ] **Step 6: Re-export the types**

In `hyperdb-api/src/lib.rs:217`, change:

```rust
pub use kv_store::KvStore;
```

to:

```rust
pub use kv_store::{BatchSetOutcome, KvStore, SetOutcome};
```

- [ ] **Step 7: Fix cross-crate `set()` callers (compile break — MUST be in this commit)**

In-workspace consumers of the reshaped methods are: the two KV test files, `hyperdb-api/tests/kv_store_in_tests.rs`, the `[[example]]` bench `hyperdb-api/benches/kv_benchmark.rs`, `hyperdb-api/README.md`, and — critically — the **`hyperdb-mcp` crate**. All the API-crate callers use `set`/`set_batch` in statement position, so they still compile (the `Result` is `?`-unwrapped and the outcome dropped); the bench and `kv_store_in_tests.rs` are covered by Task 6 Step 5's `--all-targets`/`cargo test -p hyperdb-api` gate; the README refresh is folded into Step 7b below.

**The one caller that breaks compilation is the MCP `kv_set` handler** (`hyperdb-mcp/src/server.rs:3133-3136`), which today does `kv.set(...).map_err(McpError::from)` as the closure tail and then `match result { Ok(()) => ... }`. The instant `set()` returns `Result<SetOutcome>`, `with_engine`'s inferred `R` becomes `SetOutcome` and the `Ok(())` arm is a hard type error (E0308) — breaking the whole `hyperdb-mcp` crate until Task 8. Because this is a `feat(kv)!` break, the caller fix belongs in **this** commit (an API break must leave every in-workspace caller compiling). Make the minimal one-arm edit now; Task 8 does the full handler overhaul:

```rust
        match result {
            Ok(_) => Self::ok_content(json!({ "stored": true, "store": p.store, "key": p.key })),
            Err(e) => Self::err_content(e),
        }
```

(Change only `Ok(())` → `Ok(_)` on `server.rs:3136`; leave the response body as-is for now. There is no `set_batch`/`set_as`/`set_if_absent` caller in `hyperdb-mcp`, so this is the only site.)

- [ ] **Step 7b: Refresh the README usage example**

`hyperdb-api/README.md` (KV example, ~lines 409-417) calls `kv.set(...)` / `kv.set_as(...)` / `kv.set_batch(...)` in statement position. The blocks are NOT doctested (`lib.rs` does not `include_str!` the README), so they still compile — but they no longer showcase the new return values the whole change is about. Update the example to bind the outcome, e.g. `let outcome = kv.set("theme", "dark")?; assert!(outcome.created);` and a `set_batch` line reading `BatchSetOutcome { created, overwritten }`. This is the one consumer no gate compiles, so it must be updated by hand.

- [ ] **Step 8: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests`
Expected: PASS (new tests green; existing `set_then_get_and_overwrite` still green).

Then confirm the cross-crate caller fix kept `hyperdb-mcp` compiling:

Run: `cargo build -p hyperdb-mcp`
Expected: SUCCESS (the `Ok(_)` arm from Step 7 resolves the `SetOutcome` return; no E0308).

- [ ] **Step 9: Commit**

```bash
git add hyperdb-api/src/kv_store.rs hyperdb-api/src/lib.rs hyperdb-api/tests/kv_store_tests.rs \
        hyperdb-api/README.md hyperdb-mcp/src/server.rs
git commit -m "feat(kv)!: sync set/set_as/set_batch return SetOutcome/BatchSetOutcome"
```

### Task 2: async twin — reshape `AsyncKvStore::set`/`set_as`/`set_batch`/`upsert`

Mirror Task 1 in the async file. The outcome types already exist (re-exported); this only changes `async_kv_store.rs`. Note the async `set_batch` uses a **different loop shape** than sync (mutable `inner` + `break`), which must be preserved.

**Files:**
- Modify: `hyperdb-api/src/async_kv_store.rs:100-146` (`set`, `upsert`), `:162-171` (`set_as`), `:331-358` (`set_batch`)
- Test: `hyperdb-api/tests/async_kv_store_tests.rs`

**Interfaces:**
- Consumes: `SetOutcome`, `BatchSetOutcome` from Task 1.
- Produces: `AsyncKvStore::set(&self, &str, &str) -> Result<SetOutcome>`; `set_as<T: Serialize>(&self, &str, &T) -> Result<SetOutcome>`; `set_batch(&self, &[(&str,&str)]) -> Result<BatchSetOutcome>`; private `upsert(&self, &str, &str) -> Result<bool>`.

- [ ] **Step 1: Write the failing test**

Add to `hyperdb-api/tests/async_kv_store_tests.rs`:

```rust
#[tokio::test(flavor = "current_thread")]
async fn async_set_reports_created_and_batch_outcome() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_outcome").await?;
    let kv = conn.kv_store("outcome").await?;
    assert!(kv.set("k", "v1").await?.created);
    assert!(!kv.set("k", "v2").await?.created);

    kv.set("a", "1").await?; // pre-existing
    let out = kv.set_batch(&[("a", "10"), ("b", "20")]).await?;
    assert_eq!(out.created, 1);
    assert_eq!(out.overwritten, 1);
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test async_kv_store_tests async_set_reports_created`
Expected: FAIL — `no field created on ()`.

- [ ] **Step 3: Import the outcome types**

At the top of `hyperdb-api/src/async_kv_store.rs`, add `SetOutcome` and `BatchSetOutcome` to the existing `use crate::...` / `use super::...` import of `kv_store` items (they are defined in `kv_store.rs`; import via `use crate::kv_store::{BatchSetOutcome, SetOutcome};` if not already re-imported).

- [ ] **Step 4: Reshape async `upsert` (return `updated == 0`)**

Change `async_kv_store.rs:115-146` return type to `Result<bool>` and the tail `Ok(())` to `Ok(updated == 0)`:

```rust
    async fn upsert(&self, key: &str, value: &str) -> Result<bool> {
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
        Ok(updated == 0)
    }
```

- [ ] **Step 5: Reshape async `set`, `set_as`, `set_batch`**

`set` (`:105-108`):

```rust
    pub async fn set(&self, key: &str, value: &str) -> Result<SetOutcome> {
        validate_kv_name(key, "key")?;
        Ok(SetOutcome { created: self.upsert(key, value).await? })
    }
```

`set_as` (`:167-171`):

```rust
    pub async fn set_as<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<SetOutcome> {
        validate_kv_name(key, "key")?;
        let json = serde_json::to_string(value).map_err(|e| Error::serialization(e.to_string()))?;
        Ok(SetOutcome { created: self.upsert(key, &json).await? })
    }
```

`set_batch` (`:339-358`) — preserve the mutable-`inner`+`break` shape, accumulating into the outcome:

```rust
    pub async fn set_batch(&self, entries: &[(&str, &str)]) -> Result<BatchSetOutcome> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw().await?;
        let mut inner: Result<BatchSetOutcome> = Ok(BatchSetOutcome { created: 0, overwritten: 0 });
        for (key, value) in entries {
            match self.upsert(key, value).await {
                Ok(true) => {
                    if let Ok(o) = inner.as_mut() {
                        o.created += 1;
                    }
                }
                Ok(false) => {
                    if let Ok(o) = inner.as_mut() {
                        o.overwritten += 1;
                    }
                }
                Err(e) => {
                    inner = Err(e);
                    break;
                }
            }
        }
        match &inner {
            Ok(_) => self.connection.commit_raw().await?,
            Err(_) => {
                let _ = self.connection.rollback_raw().await;
            }
        }
        inner
    }
```

- [ ] **Step 6: Fix the existing `async_kv_full_surface` test**

`async_kv_store_tests.rs:39,46,53` call `set`/`set_as`/`set_batch` in `?`-statement position — they still compile (return value dropped). No change needed; confirm in Step 7.

- [ ] **Step 7: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test async_kv_store_tests`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add hyperdb-api/src/async_kv_store.rs hyperdb-api/tests/async_kv_store_tests.rs
git commit -m "feat(kv)!: async set/set_as/set_batch return SetOutcome/BatchSetOutcome"
```

### Task 3: sync `set_if_absent` (write guard, no race)

Add a non-breaking write guard that inserts only if the key is absent, in a single statement (no check-then-write race). This is the API primitive behind MCP `overwrite:false` (Task 5).

**Files:**
- Modify: `hyperdb-api/src/kv_store.rs` (add method after `set_as`, around `:253`)
- Test: `hyperdb-api/tests/kv_store_tests.rs`

**Interfaces:**
- Produces: `KvStore::set_if_absent(&self, key: &str, value: &str) -> Result<bool>` — `true` if written, `false` if the key already existed (nothing written).
- Consumed by: Task 5 (MCP `overwrite:false`), Task 6 (async twin).

- [ ] **Step 1: Write the failing test**

Add to `hyperdb-api/tests/kv_store_tests.rs`:

```rust
#[test]
fn set_if_absent_guards_existing_key() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("guard")?;
    assert!(kv.set_if_absent("k", "first")?, "absent key must be written");
    assert!(!kv.set_if_absent("k", "second")?, "present key must be skipped");
    assert_eq!(kv.get("k")?, Some("first".to_string()), "value must be unchanged");
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests set_if_absent_guards`
Expected: FAIL — `no method named set_if_absent`.

- [ ] **Step 3: Implement `set_if_absent`**

Add after `set_as` in `hyperdb-api/src/kv_store.rs`. It reuses the conditional-INSERT half of the upsert idiom; the row count tells us whether a row was inserted:

```rust
    /// Inserts `value` under `key` only if `key` is absent.
    ///
    /// Returns `true` if a row was written, `false` if the key already existed
    /// (in which case nothing is written). A single `INSERT ... WHERE NOT
    /// EXISTS` statement decides, so there is no check-then-write race.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if `key` is invalid.
    /// - [`Error::FeatureNotSupported`] on gRPC transport.
    /// - [`Error::Server`] if the `INSERT` fails.
    pub fn set_if_absent(&self, key: &str, value: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let store = self.store_name.as_str();
        let inserted = self.connection.command_params(
            &format!(
                "INSERT INTO {t} (store_name, key, value) \
                 SELECT $1, $2, $3 \
                 WHERE NOT EXISTS (SELECT 1 FROM {t} WHERE store_name = $4 AND key = $5)",
                t = self.table_ref
            ),
            &[&store, &key, &value, &store, &key],
        )?;
        Ok(inserted > 0)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests set_if_absent_guards`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add hyperdb-api/src/kv_store.rs hyperdb-api/tests/kv_store_tests.rs
git commit -m "feat(kv): add sync KvStore::set_if_absent write guard"
```

### Task 4: sync `byte_size` + `entries` (size reporting + read-all)

Add `byte_size()` (total value bytes, for MCP `kv_size.bytes` in Task 5) and `entries()` (key+value pairs, for MCP `kv_list values:true` in Task 10). Both are non-breaking reads.

**Files:**
- Modify: `hyperdb-api/src/kv_store.rs` (add methods after `keys`, around `:333`)
- Test: `hyperdb-api/tests/kv_store_tests.rs`

**Interfaces:**
- Produces: `KvStore::byte_size(&self) -> Result<i64>` (`COALESCE(SUM(OCTET_LENGTH(value)), 0)`; 0 for empty store); `KvStore::entries(&self) -> Result<Vec<(String, String)>>` (key+value, sorted by key ascending; mirrors `keys()` streaming).
- Consumed by: Task 5 (`kv_size.bytes`), Task 10 (`kv_list values:true`), Task 6 (async twins).

- [ ] **Step 1: Write the failing test**

Add to `hyperdb-api/tests/kv_store_tests.rs`:

```rust
#[test]
fn byte_size_and_entries() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("sized")?;
    assert_eq!(kv.byte_size()?, 0, "empty store has 0 bytes");
    kv.set("a", "hello")?; // 5 bytes
    kv.set("b", "worlds")?; // 6 bytes
    assert_eq!(kv.byte_size()?, 11, "sum of OCTET_LENGTH");
    assert_eq!(
        kv.entries()?,
        vec![
            ("a".to_string(), "hello".to_string()),
            ("b".to_string(), "worlds".to_string()),
        ],
        "entries sorted by key with values"
    );
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests byte_size_and_entries`
Expected: FAIL — `no method named byte_size` / `entries`.

- [ ] **Step 3: Implement `byte_size` and `entries`**

Add after `keys` in `hyperdb-api/src/kv_store.rs`:

```rust
    /// Returns the total byte length of all values in this store
    /// (`SUM(OCTET_LENGTH(value))`). Returns 0 for an empty store; `NULL`
    /// values contribute 0 via `COALESCE`.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn byte_size(&self) -> Result<i64> {
        let sql = format!(
            "SELECT COALESCE(SUM(OCTET_LENGTH(value)), 0) FROM {} WHERE store_name = $1",
            self.table_ref
        );
        Ok(self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])?
            .scalar::<i64>()?
            .unwrap_or(0))
    }

    /// Returns this store's `(key, value)` pairs, sorted by key ascending.
    ///
    /// Materializes the whole store — intended for small scratchpad stores.
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn entries(&self) -> Result<Vec<(String, String)>> {
        let sql = format!(
            "SELECT key, value FROM {} WHERE store_name = $1 ORDER BY key ASC",
            self.table_ref
        );
        let mut result = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])?;
        let mut entries = Vec::new();
        while let Some(chunk) = result.next_chunk()? {
            for row in &chunk {
                if let Some(k) = row.get::<String>(0) {
                    entries.push((k, row.get::<String>(1).unwrap_or_default()));
                }
            }
        }
        Ok(entries)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests byte_size_and_entries`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add hyperdb-api/src/kv_store.rs hyperdb-api/tests/kv_store_tests.rs
git commit -m "feat(kv): add sync KvStore::byte_size and entries"
```

### Task 5: sync `set_batch_if_absent` (atomic batch write guard)

The `overwrite:false` variant of `kv_set_many` (Task 10) needs an atomic, all-in-one-transaction batch that skips existing keys. Add it next to `set_batch`, built on the same validate-all-then-transaction shape but calling `set_if_absent` per entry and reporting `{written, skipped}`.

**Files:**
- Modify: `hyperdb-api/src/kv_store.rs` (add method after `set_batch`, around `:430`)
- Test: `hyperdb-api/tests/kv_store_tests.rs`

**Interfaces:**
- Consumes: `set_if_absent` (Task 3), `BatchSetOutcome` is NOT reused here — a distinct outcome shape is needed (`written`/`skipped`, not `created`/`overwritten`).
- Produces: `pub struct BatchGuardOutcome { pub written: usize, pub skipped: usize }` (`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`); `KvStore::set_batch_if_absent(&self, entries: &[(&str,&str)]) -> Result<BatchGuardOutcome>`.
- Consumed by: Task 10 (MCP `kv_set_many` with `overwrite:false`), Task 6 (async twin). Re-exported in Task 1's lib.rs line — **note:** add `BatchGuardOutcome` to that re-export too (see Step 4).

- [ ] **Step 1: Write the failing test**

Add to `hyperdb-api/tests/kv_store_tests.rs`:

```rust
#[test]
fn set_batch_if_absent_skips_existing() -> Result<()> {
    let tc = TestConnection::new()?;
    let kv = tc.connection.kv_store("batch_guard")?;
    kv.set("a", "orig")?; // pre-existing → must be skipped
    let out = kv.set_batch_if_absent(&[("a", "new"), ("b", "b1"), ("c", "c1")])?;
    assert_eq!(out.written, 2, "b and c are new");
    assert_eq!(out.skipped, 1, "a existed");
    assert_eq!(kv.get("a")?, Some("orig".to_string()), "existing value untouched");
    assert_eq!(kv.get("b")?, Some("b1".to_string()));
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests set_batch_if_absent_skips`
Expected: FAIL — `no method named set_batch_if_absent`.

- [ ] **Step 3: Add `BatchGuardOutcome` and `set_batch_if_absent`**

Add `BatchGuardOutcome` next to the other outcome types (near the top of the module, after `BatchSetOutcome` from Task 1):

```rust
/// Outcome of a guarded batch write (`set_batch_if_absent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchGuardOutcome {
    /// Number of keys newly inserted.
    pub written: usize,
    /// Number of keys skipped because they already existed.
    pub skipped: usize,
}
```

Add the method after `set_batch` in `hyperdb-api/src/kv_store.rs`:

```rust
    /// Inserts every absent `(key, value)` pair in one transaction, skipping
    /// keys that already exist. All keys are validated before the transaction
    /// opens, so an invalid key aborts the whole batch without writing anything.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidName`] if any key is invalid (checked before writing).
    /// - [`Error::FeatureNotSupported`] / [`Error::Server`].
    pub fn set_batch_if_absent(&self, entries: &[(&str, &str)]) -> Result<BatchGuardOutcome> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw()?;
        let result = (|| {
            let mut outcome = BatchGuardOutcome { written: 0, skipped: 0 };
            for (key, value) in entries {
                if self.set_if_absent(key, value)? {
                    outcome.written += 1;
                } else {
                    outcome.skipped += 1;
                }
            }
            Ok(outcome)
        })();
        match &result {
            Ok(_) => self.connection.commit_raw()?,
            Err(_) => {
                let _ = self.connection.rollback_raw();
            }
        }
        result
    }
```

> Note: `set_if_absent` re-validates each key, which is redundant with the pre-loop validation here but harmless (validation is a cheap in-memory check and keeps `set_if_absent` correct when called standalone).

- [ ] **Step 4: Extend the re-export**

In `hyperdb-api/src/lib.rs` (the line changed in Task 1), add `BatchGuardOutcome`:

```rust
pub use kv_store::{BatchGuardOutcome, BatchSetOutcome, KvStore, SetOutcome};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test kv_store_tests set_batch_if_absent_skips`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add hyperdb-api/src/kv_store.rs hyperdb-api/src/lib.rs hyperdb-api/tests/kv_store_tests.rs
git commit -m "feat(kv): add sync KvStore::set_batch_if_absent atomic guard"
```

### Task 6: async twins of `set_if_absent`, `byte_size`, `entries`, `set_batch_if_absent`

Mirror Tasks 3–5 in `async_kv_store.rs`. `BatchGuardOutcome` already exists (Task 5). The async `set_batch_if_absent` uses the same mutable-`inner`+`break` loop shape as the async `set_batch` (Task 2), NOT the sync closure form.

**Files:**
- Modify: `hyperdb-api/src/async_kv_store.rs` (mirror the sync additions)
- Test: `hyperdb-api/tests/async_kv_store_tests.rs`

**Interfaces:**
- Consumes: `BatchGuardOutcome` (Task 5).
- Produces: `AsyncKvStore::set_if_absent(&self, &str, &str) -> Result<bool>`; `byte_size(&self) -> Result<i64>`; `entries(&self) -> Result<Vec<(String, String)>>`; `set_batch_if_absent(&self, &[(&str,&str)]) -> Result<BatchGuardOutcome>`.

- [ ] **Step 1: Write the failing test**

Add to `hyperdb-api/tests/async_kv_store_tests.rs`:

```rust
#[tokio::test(flavor = "current_thread")]
async fn async_guard_size_and_entries() -> Result<()> {
    let (_hyper, conn) = fresh_async_conn("async_kv_guard").await?;
    let kv = conn.kv_store("g").await?;
    assert!(kv.set_if_absent("k", "first").await?);
    assert!(!kv.set_if_absent("k", "second").await?);
    assert_eq!(kv.get("k").await?, Some("first".to_string()));

    assert_eq!(kv.byte_size().await?, 5); // "first"
    kv.set("z", "hello").await?; // 5 more
    assert_eq!(kv.byte_size().await?, 10);
    assert_eq!(
        kv.entries().await?,
        vec![
            ("k".to_string(), "first".to_string()),
            ("z".to_string(), "hello".to_string()),
        ]
    );

    let out = kv.set_batch_if_absent(&[("k", "x"), ("new", "n1")]).await?;
    assert_eq!(out.written, 1);
    assert_eq!(out.skipped, 1);
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test async_kv_store_tests async_guard_size_and_entries`
Expected: FAIL — `no method named set_if_absent`.

- [ ] **Step 3: Implement the async twins**

Add to `impl AsyncKvStore`, mirroring the sync bodies with `.await` on each `command_params`/`query_params`. `set_if_absent`:

```rust
    pub async fn set_if_absent(&self, key: &str, value: &str) -> Result<bool> {
        validate_kv_name(key, "key")?;
        let store = self.store_name.as_str();
        let inserted = self
            .connection
            .command_params(
                &format!(
                    "INSERT INTO {t} (store_name, key, value) \
                     SELECT $1, $2, $3 \
                     WHERE NOT EXISTS (SELECT 1 FROM {t} WHERE store_name = $4 AND key = $5)",
                    t = self.table_ref
                ),
                &[&store, &key, &value, &store, &key],
            )
            .await?;
        Ok(inserted > 0)
    }
```

`byte_size` and `entries` (mirror sync; `entries` streams via `next_chunk().await`):

```rust
    pub async fn byte_size(&self) -> Result<i64> {
        let sql = format!(
            "SELECT COALESCE(SUM(OCTET_LENGTH(value)), 0) FROM {} WHERE store_name = $1",
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

    pub async fn entries(&self) -> Result<Vec<(String, String)>> {
        let sql = format!(
            "SELECT key, value FROM {} WHERE store_name = $1 ORDER BY key ASC",
            self.table_ref
        );
        let mut result = self
            .connection
            .query_params(&sql, &[&self.store_name.as_str()])
            .await?;
        let mut entries = Vec::new();
        while let Some(chunk) = result.next_chunk().await? {
            for row in &chunk {
                if let Some(k) = row.get::<String>(0) {
                    entries.push((k, row.get::<String>(1).unwrap_or_default()));
                }
            }
        }
        Ok(entries)
    }
```

> Verify the async result API shape against the existing async `keys`/`size` methods before writing — match whether `scalar`/`next_chunk` are `.await`ed in this codebase (the sync twin is synchronous; the async `AsyncQueryResult` methods are `async`). Adjust the `.await` placement to match the neighbours exactly.

`set_batch_if_absent` — mutable-`inner`+`break` shape (matching async `set_batch` from Task 2):

```rust
    pub async fn set_batch_if_absent(&self, entries: &[(&str, &str)]) -> Result<BatchGuardOutcome> {
        for (key, _) in entries {
            validate_kv_name(key, "key")?;
        }
        self.connection.begin_transaction_raw().await?;
        let mut inner: Result<BatchGuardOutcome> = Ok(BatchGuardOutcome { written: 0, skipped: 0 });
        for (key, value) in entries {
            match self.set_if_absent(key, value).await {
                Ok(true) => {
                    if let Ok(o) = inner.as_mut() {
                        o.written += 1;
                    }
                }
                Ok(false) => {
                    if let Ok(o) = inner.as_mut() {
                        o.skipped += 1;
                    }
                }
                Err(e) => {
                    inner = Err(e);
                    break;
                }
            }
        }
        match &inner {
            Ok(_) => self.connection.commit_raw().await?,
            Err(_) => {
                let _ = self.connection.rollback_raw().await;
            }
        }
        inner
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api --test async_kv_store_tests`
Expected: PASS.

- [ ] **Step 5: Full-crate gate + commit**

```bash
HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-api
cargo clippy -p hyperdb-api --all-targets --all-features -- -D warnings
cargo fmt -p hyperdb-api
git add hyperdb-api/src/async_kv_store.rs hyperdb-api/tests/async_kv_store_tests.rs
git commit -m "feat(kv): add async set_if_absent/byte_size/entries/set_batch_if_absent"
```

### Task 7: error.rs — `PermissionDenied` I/O mapping + fix misleading JSON suggestions

Two independent fidelity fixes, both unit-testable in `error.rs` without a live `hyperd`: (a) an I/O-error → `McpError` mapper that preserves `PermissionDenied` instead of collapsing every file-read error to `FileNotFound`; (b) correct the `suggestion` on the JSON-in-TEXT errors so the LLM is told to cast (`value::json`) rather than to split its statement.

**Files:**
- Modify: `hyperdb-mcp/src/error.rs` (add `from_io_error`; adjust the `0A000` branch; add a `42601` branch)
- Test: `hyperdb-mcp/src/error.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `McpError::from_io_error(err: &std::io::Error, context: &str) -> McpError` — `PermissionDenied` → `ErrorCode::PermissionDenied`, `NotFound` → `ErrorCode::FileNotFound`, else `ErrorCode::InternalError`; `context` (e.g. `"value_path"`, `"load_file"`) is prefixed to the message.
- Consumed by: Task 8 (`value_path` read + `load_file` read-site).

- [ ] **Step 1: Write the failing tests**

Add (or extend the existing) `#[cfg(test)] mod tests` at the bottom of `hyperdb-mcp/src/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_preserves_permission_denied() {
        let e = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(McpError::from_io_error(&e, "value_path").code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn io_error_maps_not_found() {
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(McpError::from_io_error(&e, "value_path").code, ErrorCode::FileNotFound);
    }

    #[test]
    fn json_value_error_suggests_cast_not_split() {
        let err = hyperdb_api::Error::server(
            Some("0A000".to_string()),
            "function JSON_VALUE is not implemented yet",
            None,
            None,
        );
        let mapped = McpError::from(err);
        let s = mapped.suggestion.unwrap_or_default();
        assert!(s.contains("::json"), "expected a ::json cast hint, got: {s}");
        assert!(!s.to_lowercase().contains("split"), "must not suggest splitting: {s}");
    }

    #[test]
    fn structured_type_error_suggests_cast() {
        let err = hyperdb_api::Error::server(
            Some("42601".to_string()),
            "operator ->> requires a structured data type",
            None,
            None,
        );
        let s = McpError::from(err).suggestion.unwrap_or_default();
        assert!(s.contains("::json"), "expected a ::json cast hint, got: {s}");
    }

    #[test]
    fn multi_statement_error_still_suggests_split() {
        let err = hyperdb_api::Error::server(
            Some("0A000".to_string()),
            "multi-statement queries are not supported",
            None,
            None,
        );
        let s = McpError::from(err).suggestion.unwrap_or_default();
        assert!(s.to_lowercase().contains("one sql statement"), "got: {s}");
    }
}
```

> The `Error::server` constructor is the confirmed 4-arg form `server(sqlstate: Option<String>, message: impl Into<String>, detail: Option<String>, hint: Option<String>)` (`error.rs:306`) — the three calls above already match it (`&str` message via `impl Into<String>`; `detail`/`hint` = `None`). The assertions on `.code`/`.suggestion` are what the test verifies.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hyperdb-mcp --lib error::tests`
Expected: FAIL — `from_io_error` missing; JSON suggestions still say "split".

- [ ] **Step 3: Add `from_io_error`**

Add to `impl McpError` (after `with_suggestion`):

```rust
    /// Maps a filesystem [`std::io::Error`] to an [`McpError`], preserving the
    /// distinction between a missing file and a permission problem instead of
    /// collapsing both to [`ErrorCode::FileNotFound`].
    #[must_use]
    pub fn from_io_error(err: &std::io::Error, context: &str) -> Self {
        let code = match err.kind() {
            std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            std::io::ErrorKind::NotFound => ErrorCode::FileNotFound,
            _ => ErrorCode::InternalError,
        };
        McpError::new(code, format!("{context}: {err}"))
    }
```

- [ ] **Step 4: Fix the `0A000` branch and add a `42601` branch**

Replace the `"0A000" => { ... }` arm (`error.rs:146-150`) so a JSON-feature error steers toward the cast, while a genuine multi-statement error keeps the split hint. Add a `"42601"` arm for the structured-type case:

```rust
                "0A000" => {
                    // feature_not_supported — could be Hyper's "multi-part
                    // queries" OR an unimplemented function (e.g. JSON_VALUE).
                    let lower = err.to_string().to_lowercase();
                    if lower.contains("json") {
                        return McpError::new(ErrorCode::SqlError, err.to_string()).with_suggestion(
                            "JSON_VALUE is not implemented in this engine. Cast the TEXT value to json first, then use -> / ->> / JSON_EACH, e.g. `SELECT value::json ->> 'field' FROM _hyperdb_kv_store WHERE store_name = '...'`.");
                    }
                    return McpError::new(ErrorCode::SqlError, err.to_string()).with_suggestion(
                        "Hyper only accepts one SQL statement per call. Split your query into separate execute/query calls — one per statement.");
                }
                "42601" => {
                    // syntax_error — includes "requires a structured data type"
                    // when ->/->>' is applied to raw TEXT. Only steer toward the
                    // JSON cast when the message actually points at that case;
                    // otherwise leave a generic SQL error (no misleading hint).
                    let lower = err.to_string().to_lowercase();
                    if lower.contains("structured data type") || lower.contains("json") {
                        return McpError::new(ErrorCode::SqlError, err.to_string()).with_suggestion(
                            "The -> / ->> operators need a structured type. Cast the TEXT value to json first, e.g. `value::json ->> 'field'`.");
                    }
                    return McpError::new(ErrorCode::SqlError, err.to_string());
                }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p hyperdb-mcp --lib error::tests`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add hyperdb-mcp/src/error.rs
git commit -m "fix(mcp): preserve PermissionDenied and steer JSON errors to ::json cast"
```

### Task 8: MCP `kv_set` — `created` signal, `overwrite` guard, `value_path`, size warning

Overhaul the single most-used KV tool. `KvSetParams.value` becomes optional and gains `value_path` (server-side file read, exactly one of the two) and `overwrite` (default `true`; `false` → `set_if_absent`). The response reports `created`, `value_bytes`, and a soft `warning` over 1 MiB.

**Files:**
- Modify: `hyperdb-mcp/src/server.rs` — `KvSetParams` (`:844-861`), `kv_set` handler (`:3123-3139`); add a shared `KV_SOFT_SIZE_WARN_BYTES` const + a `kv_size_warning(bytes) -> Option<String>` helper near the KV handlers.
- Test: `hyperdb-mcp/tests/kv_tools_tests.rs`

**Interfaces:**
- Consumes: `KvStore::set` → `SetOutcome` (Task 1), `set_if_absent` (Task 3), `McpError::from_io_error` (Task 7), `attach::validate_input_path` (existing, `attach.rs:672`).
- Produces (const/helper reused by Task 10): `const KV_SOFT_SIZE_WARN_BYTES: usize = 1_048_576;` and `fn kv_size_warning(bytes: usize) -> Option<String>`.

- [ ] **Step 1: Write the failing tests**

Add to `hyperdb-mcp/tests/kv_tools_tests.rs`:

```rust
/// kv_set reports `created` (insert vs overwrite) and `value_bytes`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_reports_created_and_bytes() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let first = call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "hello" })).await?;
    assert_eq!(structured(&first)["created"], serde_json::json!(true));
    assert_eq!(structured(&first)["value_bytes"], serde_json::json!(5));

    let second = call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "hi" })).await?;
    assert_eq!(structured(&second)["created"], serde_json::json!(false));
    h.shutdown().await
}

/// overwrite:false skips an existing key without clobbering it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_overwrite_false_guards() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "orig" })).await?;
    let guard = call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "k", "value": "new", "overwrite": false })).await?;
    assert_eq!(structured(&guard)["stored"], serde_json::json!(false));
    assert_eq!(structured(&guard)["existed"], serde_json::json!(true));
    let got = call_tool(&h.client, "kv_get",
        serde_json::json!({ "store": "s", "key": "k" })).await?;
    assert_eq!(structured(&got)["value"], serde_json::json!("orig"));
    h.shutdown().await
}

/// value_path reads a file's contents; neither/both value+value_path errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_value_path_reads_file() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let dir = tempfile::TempDir::new()?;
    let path = dir.path().join("payload.txt");
    std::fs::write(&path, "from-file")?;
    let abs = std::fs::canonicalize(&path)?;

    let set = call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "f", "value_path": abs.to_string_lossy() })).await?;
    assert!(!is_error(&set), "value_path set failed: {:?}", first_text(&set));
    let got = call_tool(&h.client, "kv_get",
        serde_json::json!({ "store": "s", "key": "f" })).await?;
    assert_eq!(structured(&got)["value"], serde_json::json!("from-file"));

    // Neither value nor value_path → INVALID_ARGUMENT.
    let neither = call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "x" })).await?;
    assert!(is_error(&neither));
    // Both → INVALID_ARGUMENT.
    let both = call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "y", "value": "v", "value_path": abs.to_string_lossy() })).await?;
    assert!(is_error(&both));
    h.shutdown().await
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests kv_set_`
Expected: FAIL — no `created`/`value_bytes`/`existed` fields; `value_path` unknown field.

- [ ] **Step 3: Reshape `KvSetParams`**

Replace the `value` field and add `value_path` + `overwrite` in `hyperdb-mcp/src/server.rs:843-861`:

```rust
/// Parameters for `kv_set` (write a value under store + key).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvSetParams {
    /// Namespace of the KV store. Created on first write.
    pub store: String,
    /// Key to write.
    pub key: String,
    /// Value to store. Any string, including a JSON document. Provide exactly
    /// one of `value` or `value_path`.
    pub value: Option<String>,
    /// Absolute path to a file whose contents become the value (read
    /// server-side). Provide exactly one of `value` or `value_path`. Reads any
    /// path the server process can read — no sandbox.
    pub value_path: Option<String>,
    /// When false, do not overwrite an existing key: if the key already exists
    /// the write is skipped and the response reports `stored:false,
    /// existed:true`. Defaults to true (upsert).
    pub overwrite: Option<bool>,
    /// Target database alias. Omit (or pass `"local"`) to write to the
    /// ephemeral primary. Pass `"persistent"` to write to the durable database
    /// that survives across sessions. Other values target a user-attached
    /// database (must be writable). Each database has its own isolated stores.
    pub database: Option<String>,
    /// Shorthand for `database: "persistent"`. When true, the value is written
    /// to the persistent database. If both `database` and `persist` are set,
    /// `database` wins.
    pub persist: Option<bool>,
}
```

- [ ] **Step 4: Add the size-warning const + helper**

Near the top of the KV-handler region in `server.rs` (module scope, e.g. just before the `impl` block's KV tools, or with the other consts), add:

```rust
/// Soft threshold (bytes) above which a single KV value triggers a non-fatal
/// `warning` in the write response. The write always succeeds.
const KV_SOFT_SIZE_WARN_BYTES: usize = 1_048_576;

/// Returns a soft-size advisory when `bytes` exceeds the KV scratchpad
/// threshold, else `None`. Reports the raw byte count (no float division, to
/// stay clear of `cast_precision_loss` under the pedantic lint gate).
fn kv_size_warning(bytes: usize) -> Option<String> {
    (bytes > KV_SOFT_SIZE_WARN_BYTES).then(|| {
        format!(
            "value is {bytes} bytes (> {KV_SOFT_SIZE_WARN_BYTES} soft limit); the KV \
             store is for small scraps — consider load_data or a real table for large payloads"
        )
    })
}
```

> `kv_size_warning`/`KV_SOFT_SIZE_WARN_BYTES` are free functions/consts, not methods — place them at module scope so both `kv_set` (Task 8) and `kv_set_many` (Task 10) can call them. If the surrounding code prefers associated items, make them `impl HyperMcpServer` associated fns instead; keep the signature identical.

- [ ] **Step 5: Rewrite the `kv_set` handler**

Replace `kv_set` (`server.rs:3123-3139`). Resolve the value from exactly one of `value`/`value_path`, branch on `overwrite`, and build the response with `created`/`value_bytes`/optional `warning`:

```rust
    fn kv_set(
        &self,
        Parameters(p): Parameters<KvSetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("kv_set") {
            return Self::err_content(e);
        }
        // Exactly one of value / value_path.
        let value = match (p.value.as_deref(), p.value_path.as_deref()) {
            (Some(v), None) => v.to_string(),
            (None, Some(path)) => {
                let canonical = match crate::attach::validate_input_path(path, "value_path") {
                    Ok(c) => c,
                    Err(e) => return Self::err_content(e),
                };
                match std::fs::read_to_string(&canonical) {
                    Ok(s) => s,
                    Err(e) => return Self::err_content(McpError::from_io_error(&e, "value_path")),
                }
            }
            (Some(_), Some(_)) => {
                return Self::err_content(McpError::new(
                    ErrorCode::InvalidArgument,
                    "provide exactly one of `value` or `value_path`, not both",
                ));
            }
            (None, None) => {
                return Self::err_content(McpError::new(
                    ErrorCode::InvalidArgument,
                    "provide either `value` or `value_path`",
                ));
            }
        };
        let value_bytes = value.len();
        let overwrite = p.overwrite.unwrap_or(true);

        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = Self::kv_open(engine, db.as_deref(), &p.store)?;
            if overwrite {
                kv.set(&p.key, &value)
                    .map(|o| (true, o.created))
                    .map_err(McpError::from)
            } else {
                kv.set_if_absent(&p.key, &value)
                    .map(|written| (written, written))
                    .map_err(McpError::from)
            }
        });
        match result {
            Ok((stored, created)) => {
                let mut body = json!({
                    "stored": stored,
                    "created": created,
                    "store": p.store,
                    "key": p.key,
                    "value_bytes": value_bytes,
                });
                if !stored {
                    body["existed"] = json!(true);
                }
                if let Some(w) = kv_size_warning(value_bytes) {
                    body["warning"] = json!(w);
                }
                Self::ok_content(body)
            }
            Err(e) => Self::err_content(e),
        }
    }
```

> `body["existed"] = json!(true)` and `body["warning"] = ...` require `body` to be a mutable `serde_json::Value::Object`; the `json!({...})` macro produces exactly that, and index-assign on a JSON object inserts the key. Confirm `serde_json::json` and `ErrorCode`/`McpError` are already in scope in `server.rs` (they are — used throughout the KV handlers).

- [ ] **Step 6: Update the `kv_set` tool description**

Update the `#[tool(description = ...)]` on `kv_set` (`server.rs:3120-3121`) to document the new behavior. Append to the existing description:

```
 Returns {stored, created, value_bytes}; `created:false` means an existing value was overwritten. Pass overwrite=false to avoid clobbering (skips + returns stored:false, existed:true). Pass value_path=<absolute path> to store a file's contents server-side instead of `value` (exactly one of value/value_path; reads any server-readable path — no sandbox).
```

- [ ] **Step 7: Update the existing roundtrip assertion**

`kv_set_get_roundtrip_and_overwrite` (`kv_tools_tests.rs:185`) asserts `structured(&set)["stored"] == true` — still true. No change needed; the new fields are additive. Confirm in Step 8.

- [ ] **Step 8: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests`
Expected: PASS (new + existing).

- [ ] **Step 9: Update the load_file read-site to use `from_io_error` (uniform fix)**

For the uniform I/O-fidelity fix promised in the spec, route the `load_file` file-read error through the new helper. Locate the site:

Run: `rg -n "read_to_string|ErrorKind|FileNotFound" hyperdb-mcp/src/server.rs | rg -i "load|read_to_string"`

At the `load_file` read that maps to `FileNotFound` (surface map: `server.rs:1840-1845`), replace the `.map_err(|e| McpError::new(ErrorCode::FileNotFound, ...))` with `.map_err(|e| McpError::from_io_error(&e, "load_file"))`. If the read is via `validate_input_path` + a subsequent `read_to_string`, only the `read_to_string` mapping changes (canonicalize failures legitimately stay `FileNotFound`). If no such direct `read_to_string` exists in `load_file` (it may delegate to the engine), skip this step and note it in the commit body.

- [ ] **Step 10: Commit**

```bash
cargo clippy -p hyperdb-mcp --all-targets --all-features -- -D warnings
cargo fmt -p hyperdb-mcp
git add hyperdb-mcp/src/server.rs hyperdb-mcp/tests/kv_tools_tests.rs
git commit -m "feat(mcp): kv_set reports created/value_bytes, adds overwrite guard + value_path"
```


### Task 9: MCP `kv_size` — add `bytes` field for total value size

Extend the `kv_size` MCP tool response to report the total byte length of all values in the store (summed from `OCTET_LENGTH(value)`). The existing `size` field (key count) stays unchanged; `bytes` is additive. Backed by the new `KvStore::byte_size()` from Task 4.

**Files:**
- Modify: `hyperdb-mcp/src/server.rs:3189-3204` (the `kv_size` handler in the `#[tool_router]` impl block); update the `#[tool(description=...)]` at `:3186-3187` to mention `{size, bytes}`.
- Test: `hyperdb-mcp/tests/kv_tools_tests.rs`

**Interfaces:**
- Consumes: `KvStore::byte_size(&self) -> Result<i64>` (Task 4).
- Produces: `kv_size` response JSON `{store, size, bytes}` — `size` remains the key count, `bytes` is the sum of `OCTET_LENGTH` over all values.

- [ ] **Step 1: Write the failing test**

Add to `hyperdb-mcp/tests/kv_tools_tests.rs` (after the existing `kv_list_size_and_list_stores` test, around line 248):

```rust
/// kv_size reports both key count and total value bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_size_reports_bytes() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "a", "value": "abc" })).await?;
    call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "b", "value": "de" })).await?;

    let size = call_tool(&h.client, "kv_size", serde_json::json!({ "store": "s" })).await?;
    assert_eq!(structured(&size)["size"], serde_json::json!(2), "two keys");
    assert_eq!(structured(&size)["bytes"], serde_json::json!(5), "3+2=5 bytes");
    h.shutdown().await
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests kv_size_reports_bytes`
Expected: FAIL — no `bytes` field in the response.

- [ ] **Step 3: Extend the `kv_size` handler to call `byte_size()`**

Replace the `kv_size` handler body in `hyperdb-mcp/src/server.rs:3189-3204` (the `#[tool_router] impl HyperMcpServer` block). The handler currently calls `kv.size()?` and returns `{store, size}`. Extend the `with_engine` closure to fetch both `size` and `byte_size`, then return `{store, size, bytes}`:

```rust
    fn kv_size(
        &self,
        Parameters(p): Parameters<KvStoreParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = Self::kv_open(engine, db.as_deref(), &p.store)?;
            let key_count = kv.size().map_err(McpError::from)?;
            let value_bytes = kv.byte_size().map_err(McpError::from)?;
            Ok(json!({
                "store": p.store,
                "size": key_count,
                "bytes": value_bytes,
            }))
        });
        match result {
            Ok(val) => Self::ok_content(val),
            Err(e) => Self::err_content(e),
        }
    }
```

- [ ] **Step 4: Update the `kv_size` tool description**

Update the `#[tool(description = ...)]` on `kv_size` (around `server.rs:3186-3187`) to document the `bytes` field. Extend the description to say:

```
Returns {store, size, bytes} where `size` is the key count and `bytes` is the total `OCTET_LENGTH` of all values (0 for empty stores).
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests kv_size`
Expected: PASS (new test + existing `kv_list_size_and_list_stores` green).

- [ ] **Step 6: Commit**

```bash
cargo clippy -p hyperdb-mcp --all-targets --all-features -- -D warnings
cargo fmt -p hyperdb-mcp
git add hyperdb-mcp/src/server.rs hyperdb-mcp/tests/kv_tools_tests.rs
git commit -m "feat(mcp): kv_size reports total value bytes"
```

### Task 10: MCP `kv_set_many` — atomic batch write with outcome reporting

Add a new MCP tool backed by the atomic `set_batch` (overwrite=true) and `set_batch_if_absent` (overwrite=false) primitives from Tasks 1/5. Batch-wide outcome (`{stored, created, overwritten}` or `{stored, created, skipped}`), total byte count, and per-entry soft-size warnings for oversized values.

**Files:**
- Modify: `hyperdb-mcp/src/server.rs` — add `KvEntry` and `KvSetManyParams` near `KvSetParams` (`:843`), add a `kv_set_many` handler in the `#[tool_router] impl` beside `kv_set` (`:3123`)
- Test: `hyperdb-mcp/tests/kv_tools_tests.rs`

**Interfaces:**
- Consumes: `KvStore::set_batch` → `BatchSetOutcome` (Task 1), `set_batch_if_absent` → `BatchGuardOutcome` (Task 5), `KV_SOFT_SIZE_WARN_BYTES` + `kv_size_warning(bytes)` (Task 8), `McpError` + `ErrorCode`.
- Produces: `struct KvEntry { key: String, value: String }`; `struct KvSetManyParams { store: String, entries: Vec<KvEntry>, overwrite: Option<bool>, database: Option<String>, persist: Option<bool> }` and the `kv_set_many` tool returning `{stored, created, overwritten/skipped, total_bytes, [warnings]}`.

- [ ] **Step 1: Write the failing tests**

Add to `hyperdb-mcp/tests/kv_tools_tests.rs`:

```rust
/// kv_set_many writes all entries atomically (overwrite=true default); reports
/// {stored, created, overwritten, total_bytes}; a mixed batch counts correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_many_writes_all() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "a", "value": "1" })).await?;

    let batch = call_tool(&h.client, "kv_set_many", serde_json::json!({
        "store": "s",
        "entries": [
            { "key": "a", "value": "10" },   // overwrite
            { "key": "b", "value": "20" },   // new
            { "key": "c", "value": "30" },   // new
        ]
    })).await?;
    assert!(!is_error(&batch), "kv_set_many failed: {:?}", first_text(&batch));
    assert_eq!(structured(&batch)["stored"], serde_json::json!(3));
    assert_eq!(structured(&batch)["created"], serde_json::json!(2));
    assert_eq!(structured(&batch)["overwritten"], serde_json::json!(1));
    assert_eq!(structured(&batch)["total_bytes"], serde_json::json!(6), "10+20+30 = 6 bytes");

    let got = call_tool(&h.client, "kv_get",
        serde_json::json!({ "store": "s", "key": "a" })).await?;
    assert_eq!(structured(&got)["value"], serde_json::json!("10"));
    h.shutdown().await
}

/// kv_set_many with overwrite=false skips existing keys, reports {stored, created, skipped}.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_many_guard_skips_existing() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "a", "value": "orig" })).await?;

    let guard = call_tool(&h.client, "kv_set_many", serde_json::json!({
        "store": "s",
        "entries": [
            { "key": "a", "value": "new" },   // skipped
            { "key": "b", "value": "b1" },    // written
        ],
        "overwrite": false
    })).await?;
    assert!(!is_error(&guard), "kv_set_many guard failed: {:?}", first_text(&guard));
    assert_eq!(structured(&guard)["stored"], serde_json::json!(1));
    assert_eq!(structured(&guard)["created"], serde_json::json!(1));
    assert_eq!(structured(&guard)["skipped"], serde_json::json!(1));
    // total_bytes is the sum of ALL submitted entry values ("new"=3 + "b1"=2),
    // an upper bound under overwrite=false: the batch-guard primitive returns
    // only counts, not which keys were actually written, so total_bytes cannot
    // subtract the skipped entry's bytes.
    assert_eq!(structured(&guard)["total_bytes"], serde_json::json!(5), "\"new\"(3) + \"b1\"(2), all submitted");

    let got = call_tool(&h.client, "kv_get",
        serde_json::json!({ "store": "s", "key": "a" })).await?;
    assert_eq!(structured(&got)["value"], serde_json::json!("orig"), "existing value untouched");
    h.shutdown().await
}

/// kv_set_many rejects empty entries with INVALID_ARGUMENT.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_set_many_empty_batch_errors() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    let empty = call_tool(&h.client, "kv_set_many", serde_json::json!({
        "store": "s",
        "entries": []
    })).await?;
    assert!(is_error(&empty), "empty entries must error");
    h.shutdown().await
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests kv_set_many`
Expected: FAIL — `unknown field kv_set_many`.

- [ ] **Step 3: Add `KvEntry` and `KvSetManyParams`**

Add near `KvSetParams` in `hyperdb-mcp/src/server.rs` (after `:861`):

```rust
/// A single key-value pair for batch writes.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvEntry {
    /// Key to write.
    pub key: String,
    /// Value to store.
    pub value: String,
}

/// Parameters for `kv_set_many` (atomic batch write).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvSetManyParams {
    /// Namespace of the KV store. Created on first write.
    pub store: String,
    /// Key-value pairs to write atomically. All keys are validated before the
    /// transaction opens, so an invalid key aborts the whole batch without
    /// writing anything. Empty `entries` is an error.
    pub entries: Vec<KvEntry>,
    /// When false, skip existing keys instead of overwriting them (written
    /// entries report `created`, skipped ones report `skipped`). Defaults to
    /// true (upsert).
    pub overwrite: Option<bool>,
    /// Target database alias. Omit (or pass `"local"`) to write to the
    /// ephemeral primary. Pass `"persistent"` to write to the durable database
    /// that survives across sessions. Other values target a user-attached
    /// database (must be writable). Each database has its own isolated stores.
    pub database: Option<String>,
    /// Shorthand for `database: "persistent"`. When true, the batch is written
    /// to the persistent database. If both `database` and `persist` are set,
    /// `database` wins.
    pub persist: Option<bool>,
}
```

- [ ] **Step 4: Add the `kv_set_many` handler**

Add in the `#[tool_router] impl HyperMcpServer` block, after the `kv_set` handler (after `:3139`):

```rust
    /// Atomic batch write to the KV scratchpad.
    #[tool(
        description = "Write multiple KV pairs atomically. All keys validated before the transaction opens, so an invalid key aborts the whole batch. Returns {stored, created, overwritten, total_bytes} when overwrite=true (default); returns {stored, created, skipped, total_bytes} when overwrite=false (guard mode — skips existing keys). Empty `entries` is an error. Omit `database` to write to the ephemeral store; pass \"persistent\" (or persist=true) or an attached alias to write elsewhere."
    )]
    fn kv_set_many(
        &self,
        Parameters(p): Parameters<KvSetManyParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = self.check_writable("kv_set_many") {
            return Self::err_content(e);
        }
        if p.entries.is_empty() {
            return Self::err_content(McpError::new(
                ErrorCode::InvalidArgument,
                "entries must not be empty",
            ));
        }

        // Build the &[(&str, &str)] slice for the API crate; collect per-entry
        // warnings for oversized values.
        let pairs: Vec<(&str, &str)> = p
            .entries
            .iter()
            .map(|e| (e.key.as_str(), e.value.as_str()))
            .collect();
        let total_bytes: usize = p.entries.iter().map(|e| e.value.len()).sum();
        let mut warnings: Vec<serde_json::Value> = Vec::new();
        for entry in &p.entries {
            if let Some(w) = kv_size_warning(entry.value.len()) {
                warnings.push(json!({ "key": entry.key, "warning": w }));
            }
        }

        // Shape the outcome JSON *inside* each branch so the `with_engine`
        // closure returns a single type (`Result<Value, McpError>`). The two
        // batch primitives return different outcome structs (`BatchSetOutcome`
        // vs `BatchGuardOutcome`), so a bare `if`/`else` returning both would
        // not type-check — a closure, like any block, needs one return type.
        // `total_bytes` and `warnings` are engine-independent (computed above),
        // so they are spliced into the object after the closure returns. This
        // mirrors the single-type-closure pattern used by `kv_list` (Task 11).
        let overwrite = p.overwrite.unwrap_or(true);
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = Self::kv_open(engine, db.as_deref(), &p.store)?;
            if overwrite {
                let o = kv.set_batch(&pairs).map_err(McpError::from)?;
                Ok(json!({
                    "stored": o.created + o.overwritten,
                    "created": o.created,
                    "overwritten": o.overwritten,
                }))
            } else {
                let o = kv.set_batch_if_absent(&pairs).map_err(McpError::from)?;
                Ok(json!({
                    "stored": o.written,
                    "created": o.written,
                    "skipped": o.skipped,
                }))
            }
        });

        match result {
            Ok(mut body) => {
                body["total_bytes"] = json!(total_bytes);
                if !warnings.is_empty() {
                    body["warnings"] = json!(warnings);
                }
                Self::ok_content(body)
            }
            Err(e) => Self::err_content(e),
        }
    }
```

> The closure returns `Result<serde_json::Value, McpError>` in both branches — the `?` on `set_batch`/`set_batch_if_absent` forces the `McpError` error type, and both `Ok(json!(...))` arms produce a `Value`. `total_bytes` is the sum of **all submitted** entry value bytes (see the note on `total_bytes` semantics below), spliced into the object in the `Ok` arm so it is identical across both modes.

- [ ] **Step 5: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests`
Expected: PASS (new + existing).

- [ ] **Step 6: Commit**

```bash
cargo clippy -p hyperdb-mcp --all-targets --all-features -- -D warnings
cargo fmt -p hyperdb-mcp
git add hyperdb-mcp/src/server.rs hyperdb-mcp/tests/kv_tools_tests.rs
git commit -m "feat(mcp): add kv_set_many atomic batch write tool"
```

### Task 11: MCP `kv_list` — new `KvListParams` + `values` flag for whole-store reads

Move `kv_list` off the shared `KvStoreParams` onto its own `KvListParams` so the new `values` flag does not leak into `kv_size`/`kv_pop`/`kv_clear` schemas. When `values:true`, call `kv.entries()` (Task 4) and return `{store, entries:[{key,value},...]}` instead of the keys-only shape.

**Files:**
- Modify: `hyperdb-mcp/src/server.rs:863-876` (`KvStoreParams` — unchanged); add `KvListParams` after it; `:3169-3184` (`kv_list` handler); update the `kv_list` #[tool(description=...)] to document the `values` flag.
- Test: `hyperdb-mcp/tests/kv_tools_tests.rs`

**Interfaces:**
- Consumes: `KvStore::entries()` from Task 4 (returns `Vec<(String, String)>` sorted by key).
- Produces: `struct KvListParams { store: String, values: Option<bool>, database: Option<String>, persist: Option<bool> }`; `kv_list` handler signature changed to `Parameters<KvListParams>`.

- [ ] **Step 1: Write the failing tests**

Add to `hyperdb-mcp/tests/kv_tools_tests.rs`:

```rust
/// kv_list default (values absent/false) preserves the keys-only shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_list_keys_only_unchanged() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "a", "value": "v1" })).await?;
    call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "b", "value": "v2" })).await?;
    let list = call_tool(&h.client, "kv_list",
        serde_json::json!({ "store": "s" })).await?;
    let body = structured(&list);
    assert_eq!(body["store"], serde_json::json!("s"));
    assert_eq!(body["count"], serde_json::json!(2));
    assert_eq!(body["keys"], serde_json::json!(["a", "b"]));
    h.shutdown().await
}

/// kv_list with values:true returns entries with both key and value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_list_values_returns_entries() -> TestResult {
    let h = TestHarness::start(false, false).await?;
    call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "x", "value": "hello" })).await?;
    call_tool(&h.client, "kv_set",
        serde_json::json!({ "store": "s", "key": "y", "value": "world" })).await?;
    let list = call_tool(&h.client, "kv_list",
        serde_json::json!({ "store": "s", "values": true })).await?;
    let body = structured(&list);
    assert_eq!(body["store"], serde_json::json!("s"));
    let entries = body["entries"].as_array().expect("entries must be an array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["key"], serde_json::json!("x"));
    assert_eq!(entries[0]["value"], serde_json::json!("hello"));
    assert_eq!(entries[1]["key"], serde_json::json!("y"));
    assert_eq!(entries[1]["value"], serde_json::json!("world"));
    h.shutdown().await
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests kv_list_`
Expected: FAIL — `unknown field values` when `KvStoreParams` is used; the first test may already pass (existing shape) but the second will fail.

- [ ] **Step 3: Add `KvListParams` after `KvStoreParams`**

In `hyperdb-mcp/src/server.rs`, after the `KvStoreParams` definition (around `:876`), add:

```rust
/// Parameters for `kv_list` (enumerate keys, optionally with values).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KvListParams {
    /// Namespace of the KV store to list.
    pub store: String,
    /// When true, return the full `(key, value)` pairs as `entries`; when
    /// false or omitted, return only `keys` (the default behavior). Use
    /// `values:true` for whole-store reads without N×`kv_get`.
    pub values: Option<bool>,
    /// Target database alias. Omit (or pass `"local"`) for the ephemeral
    /// primary. Pass `"persistent"` for the durable database, or a
    /// user-attached alias. Each database has its own isolated stores.
    pub database: Option<String>,
    /// Shorthand for `database: "persistent"`. If both `database` and
    /// `persist` are set, `database` wins.
    pub persist: Option<bool>,
}
```

- [ ] **Step 4: Confirm `kv_size`/`kv_pop`/`kv_clear` still use `KvStoreParams`**

Search `hyperdb-mcp/src/server.rs` for the signatures of `kv_size`, `kv_pop`, and `kv_clear` (around `:3206`, `:3226`, `:3245` — exact line numbers may shift as earlier tasks land). Verify each takes `Parameters<KvStoreParams>`, not `KvListParams`. These three tools MUST NOT gain the `values` field — the split is intentional. No change needed; this is a verification step.

- [ ] **Step 5: Reshape the `kv_list` handler**

Replace the `kv_list` handler (`server.rs:3169-3184`) to accept `KvListParams` and branch on `p.values`:

```rust
    fn kv_list(
        &self,
        Parameters(p): Parameters<KvListParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let with_values = p.values.unwrap_or(false);
        let result = self.with_engine(|engine| {
            let db = self.resolve_db(engine, p.database.as_deref(), p.persist, true)?;
            let kv = Self::kv_open(engine, db.as_deref(), &p.store)?;
            if with_values {
                kv.entries().map(|pairs| (Some(pairs), None))
            } else {
                kv.keys().map(|keys| (None, Some(keys)))
            }
            .map_err(McpError::from)
        });
        match result {
            Ok((Some(entries), None)) => {
                let arr: Vec<Value> = entries
                    .into_iter()
                    .map(|(k, v)| json!({ "key": k, "value": v }))
                    .collect();
                Self::ok_content(json!({ "store": p.store, "entries": arr }))
            }
            Ok((None, Some(keys))) => {
                Self::ok_content(json!({ "store": p.store, "count": keys.len(), "keys": keys }))
            }
            Ok(_) => unreachable!("exactly one of entries/keys is Some"),
            Err(e) => Self::err_content(e),
        }
    }
```

> The `(Some(entries), None)` / `(None, Some(keys))` tuple shape avoids cloning or materializing both; the match arm reconstructs the correct response JSON. `Value` and `json!` are already in scope from the existing KV handlers. The keys-only response shape is preserved verbatim: `{store, count, keys}`.

- [ ] **Step 6: Update the `kv_list` tool description**

Locate the `#[tool(description = ...)]` attribute on `kv_list` (a few lines above `:3169`). Append to the existing description:

```
 Pass values=true to return full (key, value) pairs as an `entries` array instead of just keys — useful for reading a whole store without N×kv_get.
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `HYPERD_PATH=~/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests`
Expected: PASS (new tests green; existing `kv_list_then_delete_and_list_again` still green).

- [ ] **Step 8: Verify the existing `kv_list` integration test still passes**

Confirm `kv_list_then_delete_and_list_again` (`kv_tools_tests.rs:207-222`) still passes unchanged — it calls `kv_list` with no `values` parameter, so it gets the old shape. No code change needed; this is a verification step (covered by Step 7's full-crate run).

- [ ] **Step 9: Commit**

```bash
cargo clippy -p hyperdb-mcp --all-targets --all-features -- -D warnings
cargo fmt -p hyperdb-mcp
git add hyperdb-mcp/src/server.rs hyperdb-mcp/tests/kv_tools_tests.rs
git commit -m "feat(mcp): add values flag to kv_list for whole-store reads"
```

### Task 12: Docs + readme structural test — JSON queries, `::numeric` gotcha, batch/value_path/overwrite/values flags

This is a docs task driven by the structural test. Add `kv_set_many` to the tool-name enforcement list, then update the KV README section to document the new capabilities and the two SQL quirks LLMs hit during dogfooding.

**Files:**
- Modify: `hyperdb-mcp/tests/readme_tests.rs:56-63` (tool-name array)
- Modify: `hyperdb-mcp/src/readme.rs:187-208` (KV section)
- Test: `hyperdb-mcp/tests/readme_tests.rs` (structural test `readme_mentions_every_tool`)

**Interfaces:**
- Consumes: `kv_set_many` (Task 10), `value_path`/`overwrite` on `kv_set` (Task 8), `values` flag on `kv_list` (Task 11), byte reporting on `kv_set`/`kv_size` (Tasks 8/9).
- Produces: updated README text covering the new surface + JSON-query/numeric cast docs.

- [ ] **Step 1: Add `kv_set_many` to the tool-name array**

In `hyperdb-mcp/tests/readme_tests.rs`, add `"kv_set_many"` to the `tools` array (line 56–64). Insert it in the natural position after `kv_set`:

```rust
    let tools = [
        "query",
        "query_data",
        "query_file",
        "execute",
        "load_file",
        "load_files",
        "load_data",
        "load_iceberg",
        "describe",
        "sample",
        "inspect_file",
        "status",
        "export",
        "chart",
        "copy_query",
        "save_query",
        "delete_query",
        "set_table_metadata",
        "attach_database",
        "detach_database",
        "list_attached_databases",
        "watch_directory",
        "unwatch_directory",
        "kv_get",
        "kv_set",
        "kv_set_many",
        "kv_delete",
        "kv_list",
        "kv_list_stores",
        "kv_size",
        "kv_pop",
        "kv_clear",
        "get_readme",
    ];
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test -p hyperdb-mcp --test readme_tests readme_mentions_every_tool`
Expected: FAIL — `README missing mention of kv_set_many`.

- [ ] **Step 3: Update the KV section in readme.rs**

Replace `hyperdb-mcp/src/readme.rs:187-208` (the entire `### Key-value store` section up to and including the paragraph after `kv_clear`, stopping just before `### Introspection`). Preserve the section header and list structure; add the new tool, flags, and SQL notes:

```rust
### Key-value store (scratchpad)
- `kv_set` — save a variable / state / summary / JSON string under a
  store + key. Returns `{stored, created, value_bytes}`. Pass
  `overwrite: false` to skip writes that would clobber an existing key
  (response: `{stored: false, existed: true}`). Pass
  `value_path: <absolute path>` to store a file's contents server-side
  instead of inlining `value` (exactly one of `value` / `value_path`;
  reads any path the server process can read — no sandbox).
- `kv_set_many` — atomic batch write. Pass an `entries` array of
  `{key, value}` objects. All keys validated up front; an invalid key
  aborts the whole batch without writing anything. `overwrite: false`
  skips existing keys within the batch. Returns
  `{stored, created, overwritten, total_bytes}` (or `skipped` instead
  of `overwritten` under `overwrite: false`).
- `kv_get` — read a value by store + key.
- `kv_delete` — delete a key.
- `kv_list` — list keys in a store. Pass `values: true` to return
  `{entries: [{key, value}, ...]}` instead of `{keys: [...]}` — reads
  the whole store in one call (eliminates N×`kv_get`).
- `kv_list_stores` — list store namespaces that hold data in a database.
- `kv_size` — count keys and total value bytes in a store. Returns
  `{size, bytes}`.
- `kv_pop` — destructively read-and-remove the lowest-keyed entry
  (lexicographic key order, not insertion order).
- `kv_clear` — delete all keys in a store.

Every kv_* tool takes the same optional `database` parameter as the data
tools. Omit it and the store lives in the EPHEMERAL database (lost on
restart); pass `\"persistent\"` (or `persist: true`) to persist across
restarts, or any attached alias to target that database. Each database
has its own isolated set of stores. Enrich analytical tables with KV
metadata via LEFT JOIN — always filter `kv.store_name = '<namespace>'`
to avoid row multiplication, and keep the KV table in the same database
as the joined table. See the `hyper://schema/kv` resource for the join
template, and `KV store vs. a custom table` above for when to reach for
this instead of a real table.

**Querying JSON in a KV value:** Values are TEXT. To query JSON
structure, cast first: `SELECT value::json ->> 'field' FROM
_hyperdb_kv_store WHERE store_name = '...'`. The `->` / `->>` /
`JSON_EACH(...)` operators work AFTER the `::json` cast. Applying `->`
or `->>` to raw TEXT fails with SQLSTATE 42601 ("requires a structured
data type"). `JSON_VALUE(...)` is not implemented in this engine — use
the `::json` cast instead.

**::numeric truncation gotcha:** A bare `::numeric` cast defaults to
scale 0 and truncates decimal places. Example: `41.54178215::numeric`
→ `42`. Always specify precision and scale: `::numeric(20,10)`.
```

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cargo test -p hyperdb-mcp --test readme_tests readme_mentions_every_tool`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p hyperdb-mcp
git add hyperdb-mcp/src/readme.rs hyperdb-mcp/tests/readme_tests.rs
git commit -m "docs(mcp): document kv JSON queries, ::numeric gotcha, batch/value_path/overwrite/values"
```

### Task 13: Append per-crate changelog bullets (BREAKING + additions)

Per AGENTS.md rule 8, release-please owns the root `CHANGELOG.md`, all `Cargo.toml` versions, and `.release-please-manifest.json` — contributors only append per-crate `## [Unreleased]` bullets. The 0.6.1 → 0.7.0 bump is carried by the `feat!:` commits from Tasks 1–2; this task documents the user-visible changes in each crate's own `CHANGELOG.md`.

**Files:**
- Modify: `hyperdb-api/CHANGELOG.md:8-18` (`## [Unreleased]` section; create if absent)
- Modify: `hyperdb-mcp/CHANGELOG.md:8-53` (`## [Unreleased]` section — runs from the `## [Unreleased]` heading down to the blank line before `## [0.5.0]`; create if absent)

**Interfaces:**
- Consumes: `SetOutcome`, `BatchSetOutcome`, `BatchGuardOutcome` (Tasks 1,5); `set_if_absent`, `byte_size`, `entries`, `set_batch_if_absent` (Tasks 3–6); `kv_set_many` (Task 10); `kv_set.value_path`/`overwrite`, `kv_list.values`, `kv_size.bytes` (Tasks 8,9,11); error fixes (Task 7).
- Produces: no code; append bullets to each crate's `## [Unreleased]` section.

- [ ] **Step 1: Read both CHANGELOG files**

Run: `rg -n "## \[Unreleased\]" hyperdb-api/CHANGELOG.md hyperdb-mcp/CHANGELOG.md`

Expected: `hyperdb-api/CHANGELOG.md:8:## [Unreleased]`, `hyperdb-mcp/CHANGELOG.md:8:## [Unreleased]`. Both have the heading. Confirm current bullet count and style before appending.

- [ ] **Step 2: Append to `hyperdb-api/CHANGELOG.md`**

Replace the `## [Unreleased]` section (currently lines 8–18) with:

```markdown
## [Unreleased]

### Changed

- **BREAKING:** `KvStore::set`, `KvStore::set_as`, and `KvStore::set_batch` (plus their `AsyncKvStore` twins) now return `SetOutcome` or `BatchSetOutcome` instead of `Result<()>`, reporting whether each write created a new key or overwrote an existing one. The `created` signal eliminates silent data loss when an LLM accidentally clobbers existing KV data. Pre-0.7.0 callers that ignored the `Result` (statement-position `set("k","v")?;`) still compile unchanged; callers that bound the return (`let _ = set(...)?;`) must destructure or ignore the outcome. Released as 0.7.0 under pre-1.0 semver (the minor slot is the breaking slot).

### Added

- `KvStore::set_if_absent` / `AsyncKvStore::set_if_absent` — guarded write that inserts only if the key is absent (no check-then-write race; single `INSERT ... WHERE NOT EXISTS`). Returns `true` if written, `false` if the key already existed (nothing written).
- `KvStore::set_batch_if_absent` / `AsyncKvStore::set_batch_if_absent` — atomic batch variant of `set_if_absent`, returning `BatchGuardOutcome { written, skipped }`. All keys are validated before the transaction opens; an invalid key aborts the whole batch.
- `KvStore::byte_size` / `AsyncKvStore::byte_size` — returns the total byte length of all values in the store (`SUM(OCTET_LENGTH(value))`); 0 for an empty store.
- `KvStore::entries` / `AsyncKvStore::entries` — returns all `(key, value)` pairs sorted by key ascending, materializing the whole store. Intended for small scratchpad stores.
- `SetOutcome`, `BatchSetOutcome`, `BatchGuardOutcome` — public outcome types re-exported from `hyperdb_api` (sync + async twins).
- Key-value store API: `Connection::kv_store` / `AsyncConnection::kv_store` returning
  `KvStore` / `AsyncKvStore` handles over a fixed `_hyperdb_kv_store` table, with
  `get`/`set`/`get_as`/`set_as`/`delete`/`exists`/`size`/`keys`/`pop`/`clear`/`set_batch`,
  plus `kv_list_stores`. Adds the `Error::Serialization` variant.
- `Connection::kv_store_in(database, name)` / `kv_list_stores_in(database)` (plus the
  `AsyncConnection` twins) to open and enumerate KV stores in a specific attached
  database. The database name is identifier-escaped internally.
```

The existing KV bullets (lines 12–18 in the current file) are appended at the end because they were under `## [Unreleased]` even though they shipped in 0.6.0/0.6.1 (a promotion miss). Keep them as-is (they're already correct) and prepend the new BREAKING + additions above them.

- [ ] **Step 3: Append to `hyperdb-mcp/CHANGELOG.md`**

Replace the `## [Unreleased]` section (currently lines 8–53 — the entire section, running from the `## [Unreleased]` heading through the last TCP-keepalive `### Fixed` bullet, ending just before the blank line preceding `## [0.5.0]`) with:

```markdown
## [Unreleased]

### Added

- **`kv_set_many` tool** — atomic batch write accepting an array of `{key, value}` entries. Validates all keys before opening the transaction; an invalid key aborts the whole batch without writing anything. Default behavior (`overwrite` absent or `true`) reports `{stored, created, overwritten, total_bytes}`; with `overwrite: false`, existing keys are skipped (not errors) and the response reports `{stored, created: 0, skipped, total_bytes}`. Each oversized entry (> 1 MiB) adds a keyed `warning` to a `warnings` array.
- **`kv_set` — `value_path` parameter** — absolute path to a file whose contents become the value (read server-side). Provide exactly one of `value` or `value_path`; neither or both is `INVALID_ARGUMENT`. Reads any path the server process can read (same posture as `load_file` — no sandbox), with I/O errors preserved (`PermissionDenied` → `ErrorCode::PermissionDenied`, not collapsed to `FileNotFound`).
- **`kv_set` — `overwrite` parameter** (default `true`) — when `false`, skips the write if the key already exists (calls `set_if_absent` instead of `set`), returning `{stored: false, created: false, existed: true}` with the original value unchanged. Eliminates silent data-loss from accidental overwrites.
- **`kv_set` — `created` and `value_bytes` in response** — `created: true` means the key was newly inserted, `false` means an existing value was overwritten. `value_bytes` reports the UTF-8 byte length of the written value.
- **`kv_set` — soft size warning** — values exceeding 1 MiB trigger a non-fatal `warning` field in the response steering the LLM toward `load_data` or a real table for large payloads. The write always succeeds.
- **`kv_size` — `bytes` field** — response now includes `{store, size, bytes}` where `size` is the key count (unchanged) and `bytes` is `SUM(OCTET_LENGTH(value))` (0 for an empty store).
- **`kv_list` — `values` flag** (default `false`) — when `true`, the response includes `{store, entries: [{key, value}, ...]}` instead of `{store, keys: [...]}`, eliminating the N×`kv_get` read pattern for whole-store materialization. Backed by a single `SELECT key, value` streamed to completion; acceptable for small scratchpad stores.
- **Key-value scratchpad tools** — eight new MCP tools (`kv_set`, `kv_get`,
  `kv_delete`, `kv_list`, `kv_list_stores`, `kv_size`, `kv_pop`, `kv_clear`)
  let an LLM stash variables, state, summaries, or JSON strings under a
  `store`/`key` namespace without creating a table. Each takes the same
  optional `database`/`persist` routing as the data tools; **stores are
  ephemeral by default** (lost on server restart) and persist only when
  routed to `"persistent"` (or `persist: true`) or an attached alias. Each
  database keeps its own isolated set of stores. The mutating tools
  (`kv_set`, `kv_delete`, `kv_pop`, `kv_clear`) are disabled in read-only
  mode; the readers (`kv_get`, `kv_list`, `kv_size`, `kv_list_stores`)
  always work.
- **`hyper://schema/kv` resource** describing the `_hyperdb_kv_store`
  backing table, its indexless shape, the ephemeral-vs-persistent
  durability rule, per-database isolation, and the `LEFT JOIN` pattern for
  enriching analytical tables with KV metadata.

### Fixed

- **I/O error fidelity on `value_path` and `load_file`.** File-read errors now preserve `PermissionDenied` → `ErrorCode::PermissionDenied` instead of collapsing every I/O failure to `FileNotFound`. A missing file still maps to `FileNotFound`; any other I/O error becomes a generic `InternalError` with the OS message. (Implemented via `McpError::from_io_error` in `error.rs`.)
- **Misleading JSON-error `suggestion` text corrected.** The "requires a structured data type" (`42601`) and `JSON_VALUE`-not-implemented (`0A000`) errors previously suggested splitting the statement, which was wrong; they now advise casting the TEXT value to `json` first (`value::json ->> 'field'`), the actual fix. Applies narrowly to JSON-related cases; other `42601` syntax errors carry a generic message without a misleading hint.
- **Caller-fixable argument errors now return `INVALID_ARGUMENT`, not
  `INTERNAL_ERROR`.** An invalid identifier from the `hyperdb-api` layer —
  such as a KV `store`/`key` containing a disallowed byte or exceeding the
  512-byte limit — previously fell through to the catch-all `INTERNAL_ERROR`
  code, mislabeling a validation failure the caller can fix as a server-side
  bug. These (`InvalidName`, `InvalidTableDefinition`) now map to
  `INVALID_ARGUMENT`, which carries a self-correction suggestion. The
  human-readable message (naming the offending byte or the length) is
  unchanged.
- **README `--read-only` flag table no longer claims Hyper-format export is
  disabled.** The `--read-only` row wrongly listed "Hyper-format export" among
  the disabled operations, contradicting both the actual behavior (`export`
  has no read-only gate — a `.hyper` export is a harmless read-only file copy
  and stays allowed) and the same document's Read-Only Mode "Allowed" list.
  Docs-only correction; no behavior change.
- **TCP keepalive on the `hyperd` connection.** Connections to `hyperd` now
  enable TCP keepalive (60s idle, 10s interval, ~90s to declare a dead peer)
  instead of relying on the OS default 2-hour idle timeout. Without it, a
  long-lived idle connection that went half-open — laptop sleep, a network
  blip, or a `hyperd` that vanished without sending a FIN — would make the
  next query (including the `status` tool) block for up to two hours instead
  of failing fast and reconnecting. This is most visible since the daemon
  became resident-by-default in 0.5.0, which made long-lived idle connections
  the norm. (Fixed in `hyperdb-api-core` for both the sync and async clients.)
```

This replaces the *entire* current `## [Unreleased]` section (lines 8–53), so the block above reproduces everything that must survive: the existing KV `### Added` bullets (current lines 12–26) AND all three existing `### Fixed` bullets (current lines 28–53 — caller-fixable argument errors, README `--read-only` correction, TCP keepalive). Those are kept verbatim because they already shipped/were staged and correctly describe the base surface; the new 0.7.0 bullets are prepended within each heading. Because the replacement spans the whole section (not just lines 8–26), the existing `### Fixed` block is *moved into* the new block rather than orphaned below it — replacing only 8–26 would duplicate the three Fixed bullets.

- [ ] **Step 4: Verify the edits**

Run: `rg -n "Unreleased" hyperdb-api/CHANGELOG.md hyperdb-mcp/CHANGELOG.md`

Expected: both files show `## [Unreleased]` at line 8, followed by the new bullets. Eyeball the structure (### Changed / ### Added / ### Fixed headings match [Keep a Changelog](https://keepachangelog.com/)).

- [ ] **Step 5: Commit**

```bash
git add hyperdb-api/CHANGELOG.md hyperdb-mcp/CHANGELOG.md
git commit -m "$(cat <<'EOF'
docs(changelog): note KV breaking changes and MCP additions for 0.7.0

Per-crate CHANGELOG.md bullets only — release-please owns the root
CHANGELOG.md, all Cargo.toml versions, and .release-please-manifest.json.
The 0.6.1 → 0.7.0 bump is carried by the `feat!:` commits in this branch.

hyperdb-api: BREAKING — set/set_as/set_batch return SetOutcome/BatchSetOutcome
instead of Result<()>; added set_if_absent, set_batch_if_absent, byte_size,
entries (sync + async twins).

hyperdb-mcp: added kv_set_many tool, value_path + overwrite on kv_set, values
flag on kv_list, byte reporting on kv_set/kv_size; fixed PermissionDenied
I/O-error mapping and misleading JSON-error suggestion (now advises ::json cast
instead of statement split).
EOF
)"
```
