# KV Store — LLM-First MCP Ergonomics (Issue #192) — Design

**Status:** Approved (design)
**Date:** 2026-07-10
**Author:** Stefan Steiner (with Claude)
**Issue:** [tableau/hyper-api-rust#192](https://github.com/tableau/hyper-api-rust/issues/192)
**Base branch:** `main` @ `d2ab4fc` (KV work #182/#185/#188/#189 all merged; released in `v0.6.1`)

## Context

The `hyperdb-api` KV store (sync `KvStore` / async `AsyncKvStore` over a shared
`_hyperdb_kv_store` table) and its MCP tool surface (`kv_set`, `kv_get`,
`kv_list`, `kv_size`, `kv_delete`, `kv_pop`, `kv_clear`, `kv_list_stores`)
shipped in `v0.6.0`/`v0.6.1`. A subsequent Claude Code session used the KV store
as a working scratchpad and filed five friction points that make the tools
harder for an LLM to drive than they should be. This design fixes all five, plus
two adjacent LLM-ergonomics gaps surfaced during firsthand dogfooding of the
live MCP on 2026-07-10.

The guiding goal (user's words): *"make the MCP very flexible and powerful for
LLMs — the best database MCP ever, that LLMs have few issues trying to run."*

### The five issue items (all reproduced firsthand)

1. **`value_path`** — no way to store a file's contents as a KV value without the
   LLM first reading the whole file into its own context and pasting it back.
2. **Insert-vs-overwrite signal** — `kv_set` returns `{"stored":true}` whether it
   created a new key or silently clobbered an existing one. This is a *silent
   data-loss* footgun (the "sleeper"): an LLM cannot tell it just overwrote data,
   and has no way to *prevent* the overwrite.
3. **Byte-size reporting** — `kv_size` returns only a key *count*, so an LLM has
   no signal about how much data it is accumulating.
4. **Batch ops** — writing N keys means N `kv_set` calls; reading a whole store's
   values means `kv_list` (keys only) followed by N `kv_get` calls. Chatty in
   both directions.
5. **JSON-query + `::numeric` docs** — nothing LLM-facing documents how to query
   JSON stored in a TEXT value (`value::json` + `->`/`->>`/`JSON_EACH`), nor the
   `::numeric` scale-0 truncation gotcha. Both cost real trial-and-error.

### Two adjacent gaps (in scope per design decision)

- **Error-code fidelity** — every file-read I/O error collapses to
  `ErrorCode::FileNotFound`, even a genuine `PermissionDenied`
  (`hyperdb-mcp/src/error.rs:36`).
- **Misleading error `suggestion` text** — the "requires a structured data type"
  / `JSON_VALUE`-not-implemented errors carry a `suggestion` that tells the LLM
  to split its statement, which is wrong; the real fix is the `value::json` cast.

### Verified facts this design rests on

- `KvStore::set` / `AsyncKvStore::set` are **public and published** (re-exported
  at `hyperdb-api/src/lib.rs:203,217`; present at tags `v0.6.0` and `v0.6.1`;
  `hyperdb-api = "0.6.1"` is live on crates.io). Reshaping `set()`'s return type
  is therefore a **genuine breaking change** — accepted deliberately (see Version
  Impact). The KV store has seen little-to-no external use yet, so the breakage
  cost is judged low.
- The `created`-vs-overwritten bit is **already computed and discarded**: `upsert`
  captures the UPDATE rows-count at `hyperdb-api/src/kv_store.rs:203` and returns
  `Ok(())`. Threading it up is free.
- `set_batch` (`kv_store.rs:412`, `async_kv_store.rs:339`) already exists,
  validates-all-then-writes-in-one-transaction, and loops `upsert` per entry
  (`kv_store.rs:418`) — so a per-entry `created` bool aggregates naturally into a
  batch outcome. It is **not** currently surfaced as an MCP tool.
- `attach::validate_input_path(path, kind)` (`hyperdb-mcp/src/attach.rs:672`)
  already enforces absolute + canonicalized + no-`..`, with the same
  arbitrary-file-read posture as `load_file` (no sandbox/allowlist). Reused for
  `value_path`.
- The CHANGELOG mis-files the KV API under `## [Unreleased]` even though it
  shipped in `0.6.0`/`0.6.1` — a promotion miss, corrected as part of this work.

## Goals

- Fix all five issue items so an LLM can use the KV store with fewer round-trips
  and no silent data loss.
- Give the LLM a way to *prevent* an accidental overwrite, not just observe one.
- Keep every change consistent across the sync/async twins and the MCP layer.
- Improve error fidelity and docs so the LLM makes fewer wrong guesses.

## Non-Goals

- **No hard cap on value size.** Size reporting is observational + a soft,
  non-fatal warning. The write always succeeds.
- **No `kv_get_many` with an explicit key list.** `values:true` on `kv_list`
  covers whole-store reads; a keyed multi-get is speculative until demanded.
- **No value-column schema change.** The `value` column stays unconstrained
  `TEXT`.
- **No broader MCP-surface audit** beyond the KV tools and the two named error
  touch-ups.

---

## Version Impact

One intentional breaking change (`set()` / `set_batch()` return types) →
workspace **0.6.1 → 0.7.0**.

**How the bump actually happens (release-please owns it — do NOT hand-edit
versions).** This repo is driven by release-please (`release-please-config.json`,
`release-type: simple`, `bump-minor-pre-major: true`, `skip-github-release:
true`). Version numbers live behind `# x-release-please-start-version` markers in
each crate's `Cargo.toml` and are bumped by the tool via `extra-files`; the root
`CHANGELOG.md` is generated by the tool from Conventional Commits. Contributors
therefore **must not** hand-edit `Cargo.toml` versions, the workspace version, or
the root `CHANGELOG.md`. Instead:

- Land the breaking change as a **`feat!:`** commit (or a body with a
  `BREAKING CHANGE:` footer). Under `bump-minor-pre-major`, release-please turns
  a breaking change on a `0.x` line into a **minor** bump — `0.6.1 → 0.7.0` —
  which is exactly the intended target.
- release-please then opens a `chore(main): release 0.7.0` PR that bumps every
  `Cargo.toml` marker + workspace version and regenerates the root changelog.
- Per **AGENTS.md rule 8**, contributors still hand-append bullets to each
  affected crate's **own** `CHANGELOG.md` under `## [Unreleased]`; the maintainer
  promotes those to a dated heading at release time. That is the only changelog
  editing this work does.
- Because `skip-github-release: true`, the `vX.Y.Z` git tag and GitHub Release
  are created **by hand** after the release PR merges
  (`docs/GITHUB_OPERATIONS.md:228`) — this is the manual v0.7.0 step.

Under pre-1.0 semver the minor slot is the breaking slot; downstream users on
`^0.6` will not auto-upgrade and must opt in — acceptable given negligible
external KV usage to date.

---

## Design

### 1 · Insert-vs-overwrite signal + write guard (#2)

#### API layer (`hyperdb-api`) — breaking

Introduce two small public outcome types (both crates, re-exported from
`hyperdb-api`):

```rust
/// Outcome of a single KV write.
pub struct SetOutcome {
    /// `true` if the key did not previously exist (insert);
    /// `false` if an existing value was overwritten.
    pub created: bool,
}

/// Outcome of a batch KV write.
pub struct BatchSetOutcome {
    pub created: usize,     // keys newly inserted
    pub overwritten: usize, // keys whose prior value was replaced
}
```

Signature changes (sync + async twins):

```rust
pub fn set(&self, key: &str, value: &str) -> Result<SetOutcome>          // was Result<()>
pub fn set_as<T: Serialize>(&self, key: &str, value: &T) -> Result<SetOutcome>  // was Result<()>
pub fn set_batch(&self, entries: &[(&str,&str)]) -> Result<BatchSetOutcome>     // was Result<()>
```

Internal `upsert` changes from `Result<()>` to `Result<bool>` (returns
`created`): the UPDATE rows-count at `kv_store.rs:203` already distinguishes the
two paths — `updated == 0` means the conditional INSERT ran (created), otherwise
it was an overwrite. `set_batch` accumulates the per-entry bool into
`BatchSetOutcome`.

**Write guard — new non-breaking method (sync + async):**

```rust
/// Inserts `value` only if `key` is absent. Returns `true` if written,
/// `false` if the key already existed (nothing written).
pub fn set_if_absent(&self, key: &str, value: &str) -> Result<bool>
```

Implemented with the conditional-INSERT half of the existing upsert idiom
(`INSERT ... WHERE NOT EXISTS`), so there is **no** check-then-write race: a
single statement decides. Returns whether a row was inserted.

#### MCP layer (`hyperdb-mcp`)

`kv_set` response gains `created` (and `value_bytes`, see §3):

```json
{ "stored": true, "created": true, "store": "s", "key": "k", "value_bytes": 42 }
```

`KvSetParams` gains `overwrite: Option<bool>` (default `true`):

- `overwrite` absent/`true` → current behavior via `set()`, now reporting
  `created`.
- `overwrite:false` → calls `set_if_absent`. If the key already existed, the
  write is **skipped** and the response is:
  ```json
  { "stored": false, "created": false, "existed": true, "store": "s", "key": "k" }
  ```

The tool description documents `overwrite:false` as the way to avoid clobbering
existing data.

### 2 · `value_path` — server-side file read for `kv_set` (#1)

`KvSetParams`:

- `value` becomes `Option<String>` (was `String`).
- add `value_path: Option<String>`.
- **Exactly one** of `value` / `value_path` must be provided. Neither, or both,
  is an `INVALID_ARGUMENT` error with a clear message.

Behavior when `value_path` is given:

1. `attach::validate_input_path(&path, "value_path")` → absolute, canonicalized,
   no `..`.
2. `std::fs::read_to_string(&path)` → the value. I/O errors map through the
   improved error path (§5): `PermissionDenied` → `ErrorCode::PermissionDenied`,
   missing → `FileNotFound`, else a generic internal error.
3. Store the file's contents exactly as if passed via `value`; response includes
   `value_bytes` and the soft-size warning (§3).

**Security posture (documented, not silently inherited):** identical to
`load_file` — reads any path the *server process* can read; no sandbox or
allowlist. The tool description and README state this explicitly so an operator
running the MCP with broad filesystem access understands the exposure.

### 3 · Byte-size reporting + soft warning (#3)

#### API layer

Add to both twins:

```rust
/// Total bytes of all values in this store (`SUM(OCTET_LENGTH(value))`).
/// Returns 0 for an empty store.
pub fn byte_size(&self) -> Result<i64>
```

`OCTET_LENGTH` (not `LENGTH`) so the number reflects encoded bytes, matching the
`value_bytes` reported on writes. `NULL`-value rows contribute 0 via
`COALESCE(SUM(OCTET_LENGTH(value)), 0)`.

#### MCP layer

- `kv_size` response gains `bytes` (key `size` = count, unchanged):
  ```json
  { "store": "s", "size": 12, "bytes": 8451 }
  ```
- **Soft warning (never blocks a write):** `kv_set` and `kv_set_many` include a
  non-fatal `warning` field when a single value's byte length exceeds
  **1 MiB (1_048_576 bytes)**:
  ```json
  "warning": "value is 2.1 MiB; the KV store is intended for small scraps —
              consider load_data or a real table for large payloads"
  ```
  The write still succeeds; `warning` is purely advisory. `value_bytes` is
  computed as `value.len()` (UTF-8 byte length) at the MCP layer.

### 4 · Batch ops (#4)

#### New MCP tool: `kv_set_many`

Backed by a new `KvSetManyParams` struct:

```rust
struct KvSetManyParams {
    store: String,
    entries: Vec<KvEntry>,          // KvEntry { key: String, value: String }
    overwrite: Option<bool>,        // default true; false → all-or-per-key skip
    database: Option<String>,
    persist: Option<bool>,
}
```

- Wraps the existing atomic `set_batch` (validate-all-then-one-transaction — an
  invalid key aborts the whole batch, writing nothing).
- `overwrite:false` → uses a batch variant built on `set_if_absent` per entry
  within the one transaction; existing keys are skipped, not errors.
- Response:
  ```json
  { "stored": 5, "created": 3, "overwritten": 2, "total_bytes": 1234 }
  ```
  (Under `overwrite:false`, `overwritten` is replaced by `skipped`.)
- `total_bytes` = sum of `value_bytes` over **all submitted entries** (the UTF-8
  byte length of every `value` in the request), computed at the MCP layer before
  the batch runs. Under `overwrite:false` this is an **upper bound** on the bytes
  actually persisted, because `set_batch_if_absent` returns only aggregate counts
  (`written`/`skipped`), not which keys were written — the MCP layer cannot know
  which specific values were skipped, so it reports the total of what was
  submitted rather than a figure it cannot compute. Under the default
  (`overwrite:true`) every submitted entry is written, so `total_bytes` is exact.
  The soft-size warning fires per oversized entry (collected into a `warnings`
  array keyed by offending key).

#### Read-all: `values` flag on `kv_list`

`kv_list` moves off the shared `KvStoreParams` onto its **own** `KvListParams`
(so the new flag does not leak into `kv_size`/`kv_pop`/`kv_clear` schemas):

```rust
struct KvListParams {
    store: String,
    values: Option<bool>,           // default false → keys only (current behavior)
    database: Option<String>,
    persist: Option<bool>,
}
```

- `values` absent/`false` → `{ "store": "s", "keys": ["a","b"] }` (unchanged).
- `values:true` → `{ "store": "s", "entries": [{ "key":"a","value":"..." }, ...] }`,
  eliminating the N×`kv_get` read pattern. Backed by the existing full-store
  read path (a `SELECT key, value` streamed to completion; the store is a
  scratchpad, so full materialization is acceptable and matches `keys()`
  semantics today).

`kv_size`, `kv_pop`, `kv_clear` continue to share `KvStoreParams` unchanged.

### 5 · Docs + error polish (#5 + adjacent)

#### LLM-facing docs — added to BOTH `hyperdb-mcp/src/readme.rs` and the `KV_SCHEMA_RESOURCE` (`hyper://schema/kv`)

- **Querying JSON in a KV value.** Values are TEXT; to query JSON structure,
  cast first: `SELECT value::json ->> 'field' FROM _hyperdb_kv_store WHERE ...`.
  Call out explicitly:
  - `->` / `->>` / `JSON_EACH(...)` work **after** `::json`.
  - `->>` applied to raw TEXT fails with `42601` ("requires a structured data
    type").
  - `JSON_VALUE(...)` is **not implemented** in this engine — use the `::json`
    cast instead.
- **`::numeric` truncation gotcha.** A bare `::numeric` cast defaults to scale 0
  and truncates (`41.54178215::numeric → 42`). Always specify precision/scale:
  `::numeric(20,10)`. (Reproduced live during dogfooding.)

#### Error-code fidelity (`hyperdb-mcp/src/error.rs:36` and the `value_path`/`load_file` read paths)

Map `std::io::ErrorKind::PermissionDenied` → `ErrorCode::PermissionDenied`
instead of collapsing every file-read error to `FileNotFound`. `NotFound` stays
`FileNotFound`; anything else maps to a generic internal error. Applies to the
new `value_path` read and the existing `load_file` path so the fix is uniform.

#### Misleading `suggestion` text

Where the "requires a structured data type" (`42601`) / `JSON_VALUE`-not-
implemented errors currently suggest splitting the statement, replace the
suggestion with the correct remedy: cast the TEXT value to `json` first
(`value::json ->> '...'`). Scope this narrowly to the KV/JSON case so unrelated
`42601` errors are not mis-suggested — if a precise trigger can't be isolated,
prefer a general, non-misleading suggestion over a specific wrong one.

---

## Testing

All tests run under `HYPERD_PATH=~/dev/bin/hyperd` (real `hyperd` subprocess).

**API layer (`hyperdb-api`, sync + async):**
- `set` returns `created:true` on first write, `created:false` on overwrite.
- `set_batch` returns correct `{created, overwritten}` for a mixed batch.
- `set_if_absent` returns `true` then `false`; second call does not change the
  stored value.
- `byte_size` = 0 empty; matches `SUM(OCTET_LENGTH)` after known writes; unaffected
  by key count vs. value length.

**MCP layer (`hyperdb-mcp/tests/kv_tools_tests.rs`, via the local `TestHarness`):**
- `kv_set` response carries `created` correctly across insert then overwrite.
- `kv_set` with `overwrite:false` on an existing key → `{stored:false, existed:true}`,
  value unchanged.
- `kv_set` with `value_path` reads a temp file's contents; with a bad path →
  correct error code; with neither/both of `value`/`value_path` →
  `INVALID_ARGUMENT`.
- `value_path` on an unreadable file → `PermissionDenied` (not `FileNotFound`)
  where the platform allows constructing that case.
- `kv_size` returns `bytes` matching known content.
- oversized value (> 1 MiB) → write succeeds AND `warning` present.
- `kv_set_many` writes all entries atomically; correct `{created, overwritten}`;
  invalid key aborts the whole batch.
- `kv_list` default → keys only; `values:true` → `entries` with values.
- restart-durability test still passes for the new fields on a persistent store.

**Structural / hygiene:**
- `hyperdb-mcp/tests/readme_tests.rs` — add `kv_set_many` to the asserted
  tool-name list (`readme_tests.rs:56-63`) so README coverage is enforced.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` and
  `cargo fmt --all --check` clean (matching CI's exact command + toolchain).

**Changelog (per-crate `## [Unreleased]` bullets ONLY — release-please owns the
root changelog + all version numbers; see Version Impact):**
- Append to `hyperdb-api/CHANGELOG.md` under `## [Unreleased]`: `### Changed`
  (BREAKING: `set`/`set_as`/`set_batch` now return `SetOutcome`/`BatchSetOutcome`)
  and `### Added` (`set_if_absent`, `byte_size`, `SetOutcome`/`BatchSetOutcome`).
- Append to `hyperdb-mcp/CHANGELOG.md` under `## [Unreleased]`: `### Added`
  (`kv_set_many`, `value_path`, `overwrite`, `values` on `kv_list`, byte
  reporting) and `### Fixed` (PermissionDenied mapping, misleading suggestion).
- Do **not** hand-edit `Cargo.toml` versions, the workspace version, or the root
  `CHANGELOG.md` — the breaking change is carried by a `feat!:` commit and
  release-please performs the `0.6.1 → 0.7.0` bump. (The pre-existing
  mis-filing of released KV entries under a crate's `## [Unreleased]` is a
  cosmetic pre-existing issue; correct it opportunistically only if it does not
  expand scope.)

---

## Risks & Mitigations

- **Breaking `set()` return type.** Mitigated by: negligible external usage,
  single intentional bump to `0.7.0`, explicit BREAKING changelog note. `set_as`
  is bumped in the same wave to keep the write API internally consistent.
- **`value_path` arbitrary file read.** Same posture as `load_file` (accepted
  project-wide); mitigated by explicit documentation of the exposure rather than
  a false sense of sandboxing.
- **`values:true` on a huge store** materializes all values in one response.
  Acceptable for a scratchpad; the soft-size warnings on write already steer
  large payloads toward real tables. Not adding pagination now (YAGNI) — revisit
  if a large-store complaint appears.
- **Narrow-suggestion regression.** Rewriting the `42601` suggestion risks
  mis-firing on unrelated structured-type errors; mitigated by scoping the new
  suggestion to the JSON-cast case and preferring a general non-misleading
  message if a precise trigger can't be isolated.

---

## Files Touched (inventory)

- `hyperdb-api/src/kv_store.rs` — `SetOutcome`/`BatchSetOutcome`, `set`/`set_as`/
  `set_batch`/`upsert` signatures, `set_if_absent`, `byte_size`.
- `hyperdb-api/src/async_kv_store.rs` — async twins of all the above.
- `hyperdb-api/src/lib.rs` — re-export the two new outcome types.
- `hyperdb-api/CHANGELOG.md` — append `## [Unreleased]` bullets only (no version
  headings; release-please generates the root changelog + version numbers).
- `hyperdb-mcp/src/server.rs` — `KvSetParams` (`value` optional, `value_path`,
  `overwrite`), `KvListParams` (new, `values`), `KvSetManyParams`/`KvEntry`
  (new), `kv_set`/`kv_list`/`kv_size` handlers, new `kv_set_many` tool, size
  warning + `value_bytes`.
- `hyperdb-mcp/src/error.rs` — `PermissionDenied` mapping.
- `hyperdb-mcp/src/readme.rs` — JSON-query + `::numeric` docs, `kv_set_many`,
  `value_path`/`overwrite`/`values` documentation, `value_path` security note.
- `hyperdb-mcp/tests/kv_tools_tests.rs` — new coverage listed above.
- `hyperdb-mcp/tests/readme_tests.rs` — add `kv_set_many` to tool-name assertion.
- `hyperdb-mcp/CHANGELOG.md` — append `## [Unreleased]` Added/Fixed bullets only.
- **NOT touched by hand:** `Cargo.toml` versions, workspace version, root
  `CHANGELOG.md`, `.release-please-manifest.json` — all owned by release-please.
  The `0.6.1 → 0.7.0` bump is carried by a `feat!:` commit.
