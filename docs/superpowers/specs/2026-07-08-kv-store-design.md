# Key-Value Store for the Hyper Rust API + MCP — Design

**Status:** Approved (design), M1 plan pending
**Date:** 2026-07-08
**Author:** Stefan Steiner (with Claude)

> **Superseded (2026-07-09):** two design points below were settled differently
> during implementation and are retained here only as historical record — the
> authoritative sources are the M1/M2 plans
> (`docs/superpowers/plans/2026-07-08-kv-store-m1-api.md`,
> `docs/superpowers/plans/2026-07-09-kv-store-m2-mcp.md`):
>
> 1. **No PRIMARY KEY.** The PK-enforcement probe found Hyper *rejects* an
>    index-backed `PRIMARY KEY` on this table (`0A000`), so the shipped
>    `_hyperdb_kv_store` has **no PK/index** at all; `(store_name, key)`
>    uniqueness is enforced entirely by the tool layer's UPDATE-then-INSERT
>    upsert. Every "composite PK" reference below is stale.
> 2. **M2 PR title uses `feat:`, not `fix:`.** M1 (retitled `chore:`) and M2
>    bundle into a single `v0.6.0` minor release, so the MCP milestone ships
>    under `feat:`.

## Context

The `hyperdb-api` crate is a pure-Rust client for the Hyper database (PostgreSQL
wire protocol + Hyper extensions). Hyper is an OLAP columnar engine; it has **no
native key-value store**. This feature adds a small, ergonomic KV abstraction
*on top of* an ordinary Hyper SQL table, plus (in a second milestone) an MCP tool
surface so an LLM can use it as a frictionless persistent scratchpad.

The seed for this design was an exploratory conversation with Gemini. This spec
adjusts that sketch to fit two hard facts verified against the Hyper engine
source (`../hyper-db`) and the crate's own architecture:

1. **Hyper has no `ON CONFLICT` / `MERGE` / `UPSERT`.** Confirmed against the SQL
   grammar (`hyper/parser/sql/sql.ypp`, `SQLKeywords.hpp` — no `CONFLICT`
   keyword, `INSERT` has no upsert clause). Upsert must be emulated as
   `UPDATE`-then-conditional-`INSERT` inside a transaction — the exact idiom the
   repo already uses for its `_table_catalog` meta-table
   (`hyperdb-mcp/src/table_catalog.rs`).
2. **`query_as!` cannot be used *inside* `hyperdb-api`.** The macro lives in the
   sibling crate `hyperdb-api-derive`, which depends back on `hyperdb-api`; using
   it internally would create a dependency cycle (documented at
   `hyperdb-api/src/lib.rs:208-211`). The library implements its own queries with
   `command_params` / `query_params` / `fetch_optional_scalar`. **`query_as!`
   remains fully available to end users** querying the KV table.

### Why the `query_as!` constraint costs no runtime performance

`query_as!`'s benefit is **compile-time SQL verification**, not runtime speed.
The same SQL string with the same bound parameters travels the same wire path to
`hyperd` regardless of whether the macro or `command_params` produced it —
identical execution, identical speed. Using `command_params` internally forgoes
only compile-time validation of the library's ~8 hardcoded queries (written and
tested once), and end users lose nothing.

## Goals

- A typed, string-native KV store usable from both the sync (`Connection`) and
  async (`AsyncConnection`) APIs, following the crate's existing dual-API
  convention.
- **Multiple named stores** partitioned by a `store_name` namespace column.
- Core operations: get, set (upsert), delete, exists, size, keys, pop
  (destructive get-next), clear, and cross-store discovery (`list_stores`).
- Opt-in typed access via `serde_json` (`get_as` / `set_as`).
- A later MCP milestone exposing these as tools plus a documented SQL LEFT JOIN
  "enrichment" pattern (KV metadata ⋈ analytical tables).

## Non-Goals

- No FIFO queue / blocking semantics (`pop` is a destructive read, not a queue).
- No TTL/expiry, no watch/subscribe, no transactions spanning multiple KV calls
  exposed to the caller (each op is internally atomic; no caller-managed txn).
- No binary (`BYTES`) values in M1 — values are `TEXT` (strings, incl. JSON).
- No duplicate keys within a store (composite PK enforces uniqueness; a
  history/append variant is explicitly out of scope).
- No public table-name/location configuration in M1's surface (see Milestone 1
  §"Table targeting").

## Architecture Overview

### Backing table

A single, fixed backing table holds every named store, namespaced by
`store_name` (the "single table" approach — chosen over table-per-store):

```sql
CREATE TABLE IF NOT EXISTS _hyperdb_kv_store (
    store_name TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT,                 -- NULL allowed: a key may hold a null value
    PRIMARY KEY (store_name, key)
);
```

**Table name: `_hyperdb_kv_store`.** The `_hyperdb_` prefix matches the crate's
live convention (`HYPERDB_INTERNAL_PREFIX` in `hyperdb-mcp/src/engine.rs:1623`,
alongside `_hyperdb_saved_queries`). In M2 this makes `is_internal_table()`
(`engine.rs:1634`) auto-hide the table from the MCP `describe`/`status` listings
with zero special-casing. Hidden ≠ inaccessible: the LLM still joins it freely
once it learns the name from the readme / MCP resource.

**Why single-table (not table-per-store):**

| Concern | Single table | Table-per-store |
|---|---|---|
| Point lookup / COUNT | radix-prefix on `store_name`, then key | scan smaller table | ≈ equal |
| `list_stores` | `SELECT DISTINCT store_name` (one query) | query system catalog | single-table simpler |
| Create/drop a store | no DDL / `DELETE WHERE store_name=` | runtime `CREATE`/`DROP TABLE` | single-table simpler + safer |
| SQL shape | 100% static | dynamic `format!("… {store} …")` names | single-table safer |
| Disk reclaim on clear | `DELETE` leaves MVCC tombstones until compaction | `DROP TABLE` reclaims instantly | table-per-store wins (negligible at KV scale) |

The lookup speed is a wash at the expected scale (config / agent state /
scratchpad — thousands of rows). The single-table win is operational: fully
static SQL, no runtime DDL, no `format!`-built table names.

### PRIMARY KEY enforcement — verify empirically

> **Superseded:** the probe found Hyper *rejects* the index-backed `PRIMARY KEY`
> (`0A000`); the shipped table has no PK at all. See the note at the top of this
> doc. The section below is the original, pre-probe reasoning.

The Hyper grammar supports enforced, index-backed `PRIMARY KEY` (default index
is an Adaptive Radix Tree; see `hyper/cts/infra/RelationOptions.hpp`). However, a
comment in `hyperdb-mcp/src/saved_queries.rs` asserts "Hyper has no indexes" and
that crate enforces uniqueness application-side. **M1's first implementation step
empirically probes PK enforcement** against the pinned `hyperd` (insert a
duplicate `(store_name, key)`, expect an error). Outcome:

- **If enforced:** keep the PK; it provides both uniqueness and fast lookups.
- **If not enforced:** keep the PK as an optimization/index hint, and rely on the
  upsert emulation (below), which already guarantees single-row-per-key
  application-side.

Either way **the public API is identical** — this only affects internal
guarantees and test expectations.

### Upsert emulation

Hyper has no `ON CONFLICT`. `set` is implemented inside a transaction as:

```sql
UPDATE _hyperdb_kv_store SET value = $3 WHERE store_name = $1 AND key = $2;
-- if 0 rows affected:
INSERT INTO _hyperdb_kv_store (store_name, key, value)
SELECT $1, $2, $3
WHERE NOT EXISTS (
    SELECT 1 FROM _hyperdb_kv_store WHERE store_name = $1 AND key = $2
);
```

This mirrors `table_catalog.rs`'s proven pattern. `hyperd` serializes statements
within a transaction, so the read-modify-write is atomic.

**Parameter placeholders are an escaped-literal facade.** Hyper has no native
extended-query (`$1`/`$2`) protocol; `command_params`/`query_params` convert
positional params to safely-escaped SQL literals before sending
(`DEVELOPMENT.md` "Safe Escaping for Parameters"). Two consequences the plan must
respect:

- **Repeated placeholders must be verified.** The upsert references `$1`/`$2`/`$3`
  multiple times in one statement (UPDATE + INSERT + NOT EXISTS). The first
  implementation step verifies that repeated placeholders each substitute the
  correct literal. If the escaping layer does not support reuse, the fallback is
  to pass each param positionally as many times as it appears (e.g. `&[store,
  key, value, store, key, store, key]`) — an internal detail invisible to the
  public API.
- **Injection is already handled by escaping**, independent of the charset
  validation; the charset rule is for name hygiene, not safety.

## Milestone 1 — Rust API (`hyperdb-api`)

**The real feature. PR title uses a `feat:` prefix.**

### Public surface

Following the `Catalog`/`Inserter` convention (companion struct borrowing
`&'conn Connection`, with an async twin borrowing `&'conn AsyncConnection`):

```rust
// sync — src/kv_store.rs
impl Connection {
    /// Open a handle to a named KV store, creating the backing table if needed.
    pub fn kv_store(&self, name: &str) -> Result<KvStore<'_>>;
    /// Discover which named stores currently exist (SELECT DISTINCT store_name).
    pub fn kv_list_stores(&self) -> Result<Vec<String>>;
}

pub struct KvStore<'conn> { /* &conn, validated store_name, internal target */ }

impl<'conn> KvStore<'conn> {
    pub fn get(&self, key: &str) -> Result<Option<String>>;
    pub fn set(&self, key: &str, value: &str) -> Result<()>;          // upsert
    pub fn get_as<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>>;
    pub fn set_as<T: Serialize>(&self, key: &str, value: &T) -> Result<()>;
    pub fn delete(&self, key: &str) -> Result<bool>;                 // true if a row was removed
    pub fn exists(&self, key: &str) -> Result<bool>;
    pub fn size(&self) -> Result<i64>;                               // COUNT(*) for this store
    pub fn keys(&self) -> Result<Vec<String>>;                       // ORDER BY key
    pub fn pop(&self) -> Result<Option<(String, String)>>;           // destructive get-next
    pub fn clear(&self) -> Result<u64>;                              // DELETE all in this store; returns count
    pub fn name(&self) -> &str;
}
```

The async twin (`src/async_kv_store.rs`) exposes `AsyncConnection::kv_store` /
`kv_list_stores` returning `AsyncKvStore<'conn>` with the same method names as
`async fn`. No `Owned` (`Arc<AsyncConnection>`) variant in M1 — deferred under
YAGNI until a caller needs a spawnable handle.

**Lifetime placement.** `KvStore<'conn>` borrows `&'conn Connection`, slotting
into the crate's established single-root hierarchy (`DEVELOPMENT.md` "Lifetime
Safety Design": `Connection` owns; `Inserter`/`Catalog`/`Rowset` borrow `'conn`).
It holds no reference to `Catalog` or any other dependent — no circular
references, one `'conn` parameter. The plan adds a **compile-fail doc test**
proving a `KvStore` cannot outlive its `Connection`, matching the existing
compile-fail tests in `hyperdb-api/src/lib.rs`.

### Method semantics

- **Handle binds the store name once** — validated at `kv_store(name)`, not on
  every call.
- **`set`** — upsert via the emulation above.
- **`get`** — `SELECT value ... WHERE store_name=$1 AND key=$2`, returns
  `Ok(None)` when absent. Note: a present key with a SQL-NULL value also yields
  `Ok(None)`; M1 treats "absent" and "null value" identically at the `get` level.
- **`get_as` / `set_as`** — `serde_json` layer. `set_as` serializes `T` to a JSON
  string and stores it; `get_as` fetches the string and deserializes, mapping
  parse failures to `Error::Serialization`. `get_as` returns `Ok(None)` when the
  key is absent.
- **`delete`** — returns `true` iff a row was removed (via affected-row count).
- **`exists`** — cheap `SELECT 1 ... LIMIT 1` existence check.
- **`size`** — `SELECT COUNT(*) ... WHERE store_name=$1`, returns `i64`
  directly (no narrowing cast).
- **`keys`** — `SELECT key ... ORDER BY key ASC`, collected to `Vec<String>`.
- **`pop`** — in a transaction: `SELECT key, value ... ORDER BY key ASC LIMIT 1`,
  then `DELETE` that exact `(store_name, key)`; returns the pair or `None`.
  Atomic peek+delete.
- **`clear`** — `DELETE ... WHERE store_name=$1`; returns count removed
  (Gemini's `drop_store`, renamed — the shared table always survives).
- **`kv_list_stores`** — `SELECT DISTINCT store_name ORDER BY store_name`.

### Name validation

`store_name` and `key` must be non-empty, match `[A-Za-z0-9_.-]+`, and be at most
512 bytes. Violations return `Error::invalid_name`. SQL injection is already
impossible via parameterized queries; this rule keeps names clean and
predictable (per the LLM-ergonomics rationale). Applied to `store_name` at
`kv_store(name)`, and to `key` on every keyed call.

### Table targeting (internal in M1, used by M2)

`KvStore` internally holds an optional schema/database qualifier. M1's **public**
surface is only `kv_store(name)` (default location). A crate-internal constructor
(e.g. `KvStore::with_target(conn, name, target)`) accepts a schema/database
target; M2 uses it to route into the MCP's `persistent` attached database. This
keeps M1's public surface minimal while satisfying M2 without a later API break.

### Errors

Add one variant: `Error::Serialization(String)` (in `hyperdb-api/src/error.rs`)
with a constructor helper `Error::serialization(...)`, for `get_as`/`set_as`
failures. Reuse existing variants otherwise: `invalid_name`,
`feature_not_supported`, `Server`, etc. Do **not** introduce a separate error
enum.

### Transport gating

Write and parameterized paths are TCP-only, matching `Inserter::new` and
`query_params` (which return `Error::feature_not_supported` on gRPC). All KV
methods use parameterized queries, so the whole surface is TCP-gated.

### Dependencies

Add `serde` + `serde_json` to `hyperdb-api` (both already ubiquitous in the
workspace). `ToSqlParam` already has a `serde_json::Value` impl, confirming
`serde_json` is an acceptable dependency here.

### Testing (M1)

Integration + unit tests in `hyperdb-api/tests/`, using `HyperProcess::new()` to
start a real `hyperd` (per repo rules: no fabricated flags; capture and report
real output; a silent hang is a failure, not a pass). The fast gate is
**`make test-api`** (API only, no MCP/Node); full workspace `make test` runs in
Phase 5. Coverage:

- PK-enforcement probe (documents actual engine behavior).
- Upsert round-trip: set → get, set again (overwrite) → get.
- Null value handling.
- `get_as` / `set_as` round-trip for a struct; malformed-JSON → `Serialization`.
- `pop` ordering (alphabetical) + atomicity + `None` on empty.
- Multi-store isolation: same key in two stores stays distinct.
- Cross-store self-join with `store_name` filters (documents the M2 pattern;
  verifies no row multiplication when filters present).
- Repeated-placeholder substitution in the upsert statement (see backing-table
  §"Parameter placeholders").
- Charset/empty/length validation rejects.
- `delete` / `exists` / `size` / `keys` / `clear` / `kv_list_stores`.
- **Both** sync and async twins.
- **Compile-fail doc test:** a `KvStore` outliving its `Connection` must not
  compile (matches existing lifetime doc tests in `lib.rs`).

`cargo clippy` + `cargo fmt` before every commit. No narrowing `as` casts (repo
rule #7) — use `TryFrom` where any width conversion arises. Optionally, add a
Kani harness for the name/charset validator, alongside the existing
identifier-validation harnesses (`hyperdb-api/src/proofs.rs`) — marked optional,
not required for M1.

### Documentation surfaces to update (M1)

Per `RUST_DOCUMENTATION_STYLE.md` and DEVELOPMENT.md's own conventions:

- **Rustdoc** on every public item (summary < 15 words, `# Examples`
  `no_run`, `# Errors`, `# Panics`), registered as `mod` + `pub use` in
  `lib.rs`.
- **`hyperdb-api/README.md`** — overview-list entry + a KV sub-section
  (two-level structure), realistic example.
- **`hyperdb-api/CHANGELOG.md`** — `### Added` bullet under `## [Unreleased]`.
- **`DEVELOPMENT.md`** — add KV to the "Features Implemented" list.
- **`docs/README.md`** — no new `docs/` file is added by M1 (this spec lives
  under `docs/superpowers/specs/`), so no index entry is required; confirm
  during Phase 5.

## Milestone 2 — MCP (`hyperdb-mcp`)

**Designed here for coherence; planned & implemented separately. Minor change —
PR title uses a `fix:` prefix.**

Mirrors the existing `SavedQueryStore` pattern (`hyperdb-mcp/src/saved_queries.rs`):
a store abstraction with a `SessionStore` (in-memory, for `--ephemeral-only`) and
a `WorkspaceStore` (backs onto the `persistent` attached DB) split.

### Tools

`kv_get`, `kv_set`, `kv_delete`, `kv_list` (keys in a store), `kv_list_stores`,
`kv_size`, `kv_pop`, `kv_clear`. Each follows the repo tool template: a
`#[derive(Deserialize, JsonSchema)]` param struct with doc-commented fields, a
`#[tool(description = "...")]` handler with signature
`fn(&self, Parameters(p): Parameters<P>) -> Result<CallToolResult, rmcp::ErrorData>`,
`self.check_writable(...)` guard on mutators, a `self.with_engine(|engine| {...})`
body routed into the **`persistent`** DB by default (survives reconnects),
returning via `ok_content` / `err_content` with structured `McpError`.

Every new tool name must be added to the hardcoded array in
`hyperdb-mcp/tests/readme_tests.rs` **and** documented in
`hyperdb-mcp/src/readme.rs`, or the structural coverage test fails.

Tool descriptions frame the store as a persistent scratchpad, e.g. `kv_set`:
"Persistent scratchpad. Save variables, state, summaries, or JSON configs to
remember later without creating a database table."

### MCP Resource

Register `hyper://schema/kv` (text/plain) describing the `_hyperdb_kv_store`
shape (columns, composite PK, intent) so hosts can inject it as ambient schema
context.

### LEFT JOIN enrichment pattern

Document — in `readme.rs` and the `execute`/`query` tool descriptions — that any
analytical table can be enriched with KV metadata without `ALTER TABLE`:

```sql
SELECT t.*, kv.value AS metadata
FROM my_custom_table t
LEFT JOIN _hyperdb_kv_store kv
       ON t.id = kv.key AND kv.store_name = 'your_namespace'
WHERE t.status = 'active';
```

**Documentation must always include the `kv.store_name = '…'` filter.** Omitting
it fans out any key that exists in multiple stores (row multiplication) — a
query-authoring footgun independent of the single-table design. No new API is
needed for joins.

### Optional (stretch)

An `enrich-analytics` MCP Prompt that pre-bakes the join template. Marked a
stretch goal for M2, not required.

### CHANGELOG

Add a bullet under `## [Unreleased]` in `hyperdb-mcp/CHANGELOG.md`.

## Milestones, branches, PR titles

> **Superseded:** M2 ships under **`feat:`**, not `fix:` — M1 and M2 bundle into a
> single `v0.6.0` minor release. See the note at the top of this doc.

| Milestone | Crate | Branch | PR title prefix |
|---|---|---|---|
| M1 — API | `hyperdb-api` | current branch family | **`feat:`** (the real feature) |
| M2 — MCP | `hyperdb-mcp` | separate branch | **`fix:`** (minor surfacing) |

One design doc (this) covers both. The implementation plan written next covers
**M1 only**; M2 gets its own plan later. M1 must land and publish before M2 can
consume the new API.

## Conventions & Guidelines Compliance

All work follows [`docs/RUST_GUIDELINES.md`](../../RUST_GUIDELINES.md) (Microsoft
Pragmatic Rust) and [`docs/RUST_DOCUMENTATION_STYLE.md`](../../RUST_DOCUMENTATION_STYLE.md).
The load-bearing rules for this feature, and how the design already honors them:

**Machine-enforced (CI gates — a PR cannot merge while any fails):**

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo doc -D warnings` clean.
- **M-PUBLIC-DEBUG** — `KvStore`, `AsyncKvStore`, and any new public type derive
  `Debug` (`missing_debug_implementations = "warn"` + `-D warnings`).
- **M-CANONICAL-DOCS** — every `pub` item has a `///` summary (`missing_docs`).
- **Integer cast discipline** — `size()` returns the `COUNT(*)` `i64` directly;
  no narrowing `as`. Any width conversion uses `TryFrom` or a justified
  `#[expect(clippy::cast_*, reason = "...")]`. (Repo rule #7.)
- **M-UNSAFE** — no `unsafe` is expected in this feature; if any appears it
  carries a `// SAFETY:` comment.
- Supply-chain: `serde`/`serde_json` are permissively licensed and already in
  the lockfile — `cargo deny` / `cargo audit` stay green.

**Human-review (reviewer checklist):**

- **M-ESSENTIAL-FN-INHERENT / M-REGULAR-FN** — KV behavior lives as **inherent
  methods** on `KvStore` (and `kv_store`/`kv_list_stores` inherent on
  `Connection`), *not* a `use`-required extension trait. This is a deliberate
  improvement over Gemini's `HyperKv` trait sketch, which would have forced a
  trait import to call the methods.
- **M-CONCISE-NAMES** — `KvStore`, `get`, `set`, `pop`, `clear` describe what they
  do; no `Manager`/`Helper`/`Service` weasel words.
- **M-APP-ERROR / M-ERRORS-CANONICAL-STRUCTS** — `hyperdb-api` keeps its single
  canonical `Error` enum; the new `Serialization` variant gets a public
  constructor `Error::serialization(...)`, matching every other variant.
- **M-DONT-LEAK-TYPES** — public signatures use `std` types (`String`, `Option`,
  `Vec`, tuples). `serde` appears only as generic bounds on `get_as`/`set_as`
  (`T: Serialize` / `T: DeserializeOwned`), never as a concrete leaked type.
- **M-DOCUMENTED-MAGIC** — the key/name max-length (512) is a documented `const`,
  not an inline literal; the validation charset is a documented `const`.
- **String-arg convention** — methods take `&str` for keys/values, matching the
  crate's established `execute_command(&self, &str)` / `query_params` style
  (the crate uses `TryInto<TableName>` only for schema/table *names*). Reviewer
  confirms consistency with siblings rather than importing `impl AsRef<str>`
  wholesale.

**Documentation (M-FIRST-DOC-SENTENCE + doc-style):**

- Every public item's first doc sentence is < 15 words, on one line.
- `# Examples` (as `no_run`, since they need a live `hyperd`), `# Errors`, and
  `# Panics` sections on all public methods; intra-doc links (`[`KvStore`]`).
- Doc examples compile under `cargo test --doc`.
- `hyperdb-api/README.md` gets a KV overview entry + sub-section (two-level
  structure per the doc-style guide); implementation internals stay in rustdoc
  / `DEVELOPMENT.md`, not the README. New behavior is captured in code comments
  over prose docs (per user preference [[feedback_code_comments_over_docs]]).

## Adversarial review (Harness)

This feature is built with the **Harness** agent-team workflow — offline,
operator-gated, role-separated (`doer ≠ validator ≠ merger`). Every phase is
reviewed by independent adversarial agents that do not see the conversation
history:

- **Phase 2 — plan review:** BOTH `feature-dev:code-reviewer` (fast, line-level)
  and `system-agents:code-review` (deeper, architectural) run in parallel against
  the M1 plan file before any code is written.
- **Phase 4 — per-iteration review:** `feature-dev:code-reviewer` audits each
  committed iteration against an explicit acceptance checklist that includes the
  guideline rules above (cast discipline, `Debug`, doc sections, inherent-method
  design, canonical error).
- **Phase 5 — final pre-merge sweep:** BOTH reviewers in parallel against the
  integrated branch, plus full E2E verification (real `hyperd`, `cargo
  test`/`clippy`/`fmt`/`doc`, doc tests).

Reviewer briefs cite concrete acceptance criteria (e.g. "`size()` must return
`i64` with no `as` cast"; "`KvStore` must derive `Debug`"; "no method requires a
trait import").

## Risks

- **PK enforcement unknown until probed.** Mitigated: first implementation step
  verifies it; the upsert emulation guarantees correctness regardless. Public
  API is unaffected by the outcome.
- **`DELETE`-based `clear` leaves MVCC tombstones** until compaction. Negligible
  at KV scale; acceptable given the single-table simplicity win. Documented.
- **`serde_json` dependency added to `hyperdb-api`.** Low risk — already used
  transitively via `ToSqlParam`'s `serde_json::Value` impl.
- **Join footgun (missing `store_name` filter).** Mitigated by always including
  the filter in documented examples.

## Follow-ups (post-merge)

- Write a feature memory doc in the `~/dev/ssteiner-ai` repo once M1/M2 land, as
  done for prior features.
