# hyperdb-mcp — On-Demand Smoke Tests

Manual, tool-driven smoke tests for the `hyperdb-mcp` server. Run these
against a **live, running MCP** (e.g. from an LLM client that has the
`hyperdb` tools connected) to confirm end-to-end behavior after a build,
a config change, or before shipping a release.

These complement — they do **not** replace — the automated suites:

| Layer | Where | Runs in CI |
|---|---|---|
| End-to-end tool dispatch | [`tests/kv_tools_tests.rs`](tests/kv_tools_tests.rs), [`tests/end_to_end_mcp_tests.rs`](tests/end_to_end_mcp_tests.rs) | yes |
| Error-code mapping | [`tests/error_tests.rs`](tests/error_tests.rs) | yes |
| Tool-name coverage in the LLM README | [`tests/readme_tests.rs`](tests/readme_tests.rs) | yes |

The automated tests prove correctness in isolation with a fresh temp
server. The smoke tests here prove the **wired-up, running** server behaves
— useful because a live server carries real state (a populated persistent
database, an attached alias, read-only mode, a shared daemon) that the unit
tests deliberately don't.

---

## Safety first — the persistent database is real

The MCP has two databases per session:

- the **local** database (ephemeral and the default target; a fresh
  temp `.hyper` that is deleted on server restart), and
- the **persistent** database (`database: "persistent"` / `persist: true`)
  which is the user's durable database and **may already hold real data**.

**Rules for smoke testing:**

1. **Default to the local store.** Omit `database` on every call unless
   you are explicitly testing routing. Local writes cost nothing and
   vanish on restart.
2. **Never create, drop, or overwrite a table without checking first —
   scoped to the database you're about to write to.** A real `products`
   table with a thousand rows can already be sitting in the persistent DB.
   Before any `CREATE`/`DROP`, confirm the name is free *in that database*:
   run `describe table=<name> database=persistent` (or a
   `SELECT COUNT(*) FROM <name>` via `query database=persistent`) when the
   target is persistent — a bare `describe` inspects only the **local**
   primary and would miss a persistent collision, and `status` never lists
   table *names* (only aggregate counts), so neither alone protects you.
   Always use a `smoke_`-prefixed name for any scratch table you create.
3. **Clean up persistent scratch immediately.** If a test writes to
   `persistent` (routing/isolation checks), `kv_clear` those stores and drop
   any scratch tables the moment the assertion is made.
4. **Prefer `--ephemeral-only`** when you can start the server yourself — it
   skips the persistent attachment entirely, so there is nothing real to
   touch.

Every smoke run should end with the persistent database in exactly the state
it started in. The final section is a verification checklist for that.

---

## Preconditions

- `hyperd` available. `HYPERD_PATH` may name the executable or its containing
  directory. When it is absent or non-UTF-8, the runtime will search upward from
  the current directory for `.hyperd/current/hyperd`; it does not search the
  general executable path.
- The `hyperdb` MCP tools connected and responding.
- Confirm the server is up and note its mode before you start:

```
status
```

Expected full response: `{"hyperd_running": true, "engine_busy": false,
"default_database": "local", ..., "read_only": false, "engine":
{"mode": "daemon"|"local", ...}}`. Note `read_only` — if `true`, the five
KV **mutators** (`kv_set`, `kv_set_many`, `kv_delete`, `kv_pop`, `kv_clear`)
are expected to be **rejected** (see §7); the four readers still work.

If `engine_busy: true`, status is deliberately partial. SQL-dependent counts
are omitted and `hyperd_running: false` is inconclusive; retry after the
in-progress operation completes rather than treating the degraded response as
a definitive outage.

Throughout, `→` shows the expected JSON the tool returns. Store/key names
below all begin with `smoke` so they're easy to spot and purge.

---

## Diagnostic preflight

Before touching real data, run both native doctor presentations:

```bash
hyperdb-mcp doctor
hyperdb-mcp doctor --json
```

Doctor is side-effect-free: it creates no directories, starts no daemon or
`hyperd`, and opens or creates no database. Compare its authoritative native
MCP/Rust API identity with optional launcher-reported npm identity and any
freshly verified daemon `STATUS`. Review local paths before sharing the report.

If persistent warm-up or a persistent-routed call returns `RESOURCE_BUSY`,
confirm the message includes the effective `.hyper` path, raw diagnostic, and
SQLSTATE `55006`. Run doctor, compare identities, and close the possible owner
(another Hyper/Tableau process) or copy/select another persistent file before
retrying. Do not treat unrelated `55006` errors as lock contention.

---

## 1. Server + KV surface present

The server should expose 9 `kv_*` tools and the `hyper://schema/kv`
resource.

- `kv_*` tools: `kv_set`, `kv_set_many`, `kv_get`, `kv_delete`, `kv_list`,
  `kv_list_stores`, `kv_size`, `kv_pop`, `kv_clear`.
- Reading `hyper://schema/kv` returns text mentioning `_hyperdb_kv_store`
  and a `LEFT JOIN` template.

---

## 2. Create / read / overwrite (upsert)

```
kv_set   store=smoke key=greeting value="hello world"     → {"stored": true, "created": true, "value_bytes": 11, "store": "smoke", "key": "greeting", "resolved_database": "local"}
kv_get   store=smoke key=greeting                          → {"found": true, "value": "hello world", "resolved_database": "local"}
kv_get   store=smoke key=does_not_exist                    → {"found": false, "value": null, "resolved_database": "local"}
```

A miss is **not** an error — `found: false` with a `null` value.

Batch writes are atomic and validate every key before writing:

```
kv_set_many store=smoke_batch entries=[{"key":"batch_a","value":"A"},{"key":"batch_b","value":"B"}]
  → {"stored": 2, "created": 2, "overwritten": 0, "total_bytes": 2, "resolved_database": "local"}
kv_list store=smoke_batch
  → {"store": "smoke_batch", "count": 2, "keys": ["batch_a","batch_b"], "resolved_database": "local"}
kv_clear store=smoke_batch
  → {"store": "smoke_batch", "removed": 2, "resolved_database": "local"}
```

**Overwrite must not create a duplicate row** (the backing table is
indexless; `kv_set` is an app-side upsert):

```
kv_size  store=smoke                                       → {"store": "smoke", "size": 1, "bytes": 11, "resolved_database": "local"}
kv_set   store=smoke key=greeting value="HELLO AGAIN"      → {"stored": true, "resolved_database": "local", ...}
kv_size  store=smoke                                       → {"store": "smoke", "size": 1, "bytes": 11, "resolved_database": "local"}   # still 1, not 2
kv_get   store=smoke key=greeting                          → {"found": true, "value": "HELLO AGAIN", "resolved_database": "local"}
```

---

## 3. Listing, size, store discovery

Seed a few keys, then list:

```
kv_set store=smoke key=alpha   value=1
kv_set store=smoke key=bravo   value=2
kv_set store=smoke key=charlie value=3

kv_list        store=smoke   → {"store": "smoke", "count": 4, "keys": ["alpha","bravo","charlie","greeting"], "resolved_database": "local"}   # sorted ascending
kv_size        store=smoke   → {"store": "smoke", "size": 4, "bytes": 14, "resolved_database": "local"}
kv_list_stores               → {"count": 1, "stores": ["smoke"], "resolved_database": "local"}
```

`kv_list` keys are always sorted ascending. `kv_list_stores` reflects only
stores that currently hold rows (there is no separate store registry — an
emptied store disappears from the list; see §5).

---

## 4. Value fidelity — JSON, empty, large

```
kv_set store=smoke key=config    value='{"retries": 3, "nested": {"flag": true}}'
kv_get store=smoke key=config    → {"found": true, "value": "{\"retries\": 3, \"nested\": {\"flag\": true}}", "resolved_database": "local"}   # byte-for-byte

kv_set store=smoke key=empty_val value=""
kv_get store=smoke key=empty_val → {"found": true, "value": "", "resolved_database": "local"}    # empty string, NOT a miss

kv_set store=smoke key=big_blob  value="<a few hundred+ chars>"
kv_get store=smoke key=big_blob  → {"found": true, "value": "<same string, intact>", "resolved_database": "local"}
```

The empty-string case is the important one: `{"found": true, "value": ""}`
must stay distinct from a miss `{"found": false, "value": null}`.

---

## 5. Destructive semantics — delete, pop, clear

**Delete is idempotent and reports whether the key existed:**

```
kv_delete store=smoke key=greeting        → {"deleted": true, "resolved_database": "local", ...}   # existed
kv_delete store=smoke key=greeting        → {"deleted": false, "resolved_database": "local", ...}   # already gone — no error
kv_delete store=smoke key=never_existed   → {"deleted": false, "resolved_database": "local", ...}
```

**`kv_pop` destructively removes the lowest-keyed entry** (a work-queue
drain in ascending key order):

```
# with keys [alpha, bravo, charlie, config, empty_val, big_blob] present
kv_pop store=smoke   → {"found": true, "key": "alpha",    "value": "1",   "resolved_database": "local"}
kv_pop store=smoke   → {"found": true, "key": "big_blob", "value": "...", "resolved_database": "local"}   # 'b' < 'c'
kv_pop store=smoke   → {"found": true, "key": "bravo",    "value": "2",   "resolved_database": "local"}
```

**`kv_clear` empties the store and returns the count removed:**

```
kv_size  store=smoke   → {"store": "smoke", "size": N, "bytes": B, "resolved_database": "local"}
kv_clear store=smoke   → {"store": "smoke", "removed": N, "resolved_database": "local"}
kv_size  store=smoke   → {"store": "smoke", "size": 0, "bytes": 0, "resolved_database": "local"}
```

Here `N` is the key count immediately before the clear, and `B` is the sum
of the remaining values' UTF-8 byte lengths at that point.

**Empty-store edge cases:**

```
kv_pop   store=smoke   → {"found": false, "resolved_database": "local"}          # nothing to pop
kv_clear store=smoke   → {"store": "smoke", "removed": 0, "resolved_database": "local"}   # idempotent
kv_list_stores         → {"count": 0, "stores": [], "resolved_database": "local"}   # emptied store drops out
```

---

## 6. Input validation

`store` and `key` must be ASCII `[A-Za-z0-9_.-]`, non-empty, ≤ 512 bytes.
Violations are rejected as **`INVALID_ARGUMENT`** (not `INTERNAL_ERROR`)
with a message that names the offending byte or the actual length:

```
kv_set store=smoke      key="has a space" value=x
  → error INVALID_ARGUMENT: "invalid name: KV key contains an invalid byte 0x20; allowed: A-Z a-z 0-9 _ . -"

kv_set store="bad/store" key=k value=x
  → error INVALID_ARGUMENT: "invalid name: KV store name contains an invalid byte 0x2f; ..."

kv_set store=smoke key="<630-byte key>" value=x
  → error INVALID_ARGUMENT: "invalid name: KV key exceeds 512-byte limit (630 bytes)"
```

Boundary check: a 499-byte key is **accepted**; a 630-byte key is
**rejected**. (Automated in `error_tests.rs::maps_invalid_name_to_invalid_argument`.)

---

## 7. Read-only mode

Only relevant when the server runs with `--read-only` (`status` shows
`"read_only": true`). Start such a server yourself for this check — do not
assume the shared daemon is read-only.

```
# readers work:
kv_get store=smoke key=k    → {"found": false, "value": null, "resolved_database": "local"}
kv_list store=smoke         → {"store": "smoke", "count": 0, "keys": [], "resolved_database": "local"}
kv_size store=smoke         → {"store": "smoke", "size": 0, "bytes": 0, "resolved_database": "local"}
kv_list_stores              → {"count": 0, "stores": [], "resolved_database": "local"}

# mutators are blocked:
kv_set    store=smoke key=k value=v  → error READ_ONLY_VIOLATION ("... not permitted in read-only mode")
kv_set_many store=smoke entries=[{"key":"k","value":"v"}] → error READ_ONLY_VIOLATION
kv_delete store=smoke key=k          → error READ_ONLY_VIOLATION
kv_pop    store=smoke                 → error READ_ONLY_VIOLATION
kv_clear  store=smoke                 → error READ_ONLY_VIOLATION
```

The same mode guards `execute`, `load_data`, `load_file`, `load_files`,
`load_iceberg`, `watch_directory`, `save_query`, `delete_query`,
`set_table_metadata`, `copy_query`, and writable/create `attach_database`.
Read-only attachment, `unwatch_directory`, and export in every format
(including Hyper) remain available.

---

## 8. Database routing + isolation

**⚠ Touches the persistent database. Clean up after (§12).**

Each database keeps its own isolated set of stores. The same store name in
two databases holds independent values. `persist: true` and
`database: "persistent"` target the same place.

Every successful database-routed call also returns canonical
`resolved_database`: `"local"`, `"persistent"`, or a lowercased attached
alias. Verify it on every response below. When both selectors are supplied,
an explicit `database` wins over `persist: true` (for example,
`database=local persist=true` resolves to `local`). Mixed-case
`database=PeRsIsTeNt` resolves to `persistent`; mixed-case attached aliases
resolve to the registry's lowercase alias.

```
kv_set store=smoke_routing key=where  value="local"                           # → local (default)
kv_set store=smoke_routing key=where  value="persistent" database=persistent  # → persistent
kv_set store=smoke_routing key=where2 value="via-flag"    persist=true        # → persistent (same DB)

kv_get  store=smoke_routing key=where                       → {"found": true, "value": "local", "resolved_database": "local"}
kv_get  store=smoke_routing key=where  database=persistent  → {"found": true, "value": "persistent", "resolved_database": "persistent"}
kv_get  store=smoke_routing key=where2 persist=true         → {"found": true, "value": "via-flag", "resolved_database": "persistent"}

kv_list store=smoke_routing                       → {"store": "smoke_routing", "count": 1, "keys": ["where"], "resolved_database": "local"}
kv_list store=smoke_routing database=persistent   → {"store": "smoke_routing", "count": 2, "keys": ["where","where2"], "resolved_database": "persistent"}
```

The local and persistent `where` values differ → isolation holds.
`persist=true` and `database=persistent` landed in the same store → both
keys present in persistent.

**Ephemeral-only guard:** if the server was started with `--ephemeral-only`,
`kv_set ... persist=true` returns `INVALID_ARGUMENT` (a clear error, not a
panic).

---

## 9. The `LEFT JOIN` enrichment pattern

The backing table `_hyperdb_kv_store(store_name, key, value)` is hidden from
`describe`/`status` but queryable directly. This is the point of the KV
store: annotate analytical rows with scratchpad metadata via a plain SQL
join. **Run this in the local DB** (create a `smoke_`-prefixed table):

```
kv_set store=product_notes key=P1 value="flagship - review pricing Q3"
kv_set store=product_notes key=P3 value="discontinue candidate"

execute ["CREATE TABLE smoke_products (id TEXT, name TEXT, revenue INTEGER)"]
execute ["INSERT INTO smoke_products (id,name,revenue) VALUES ('P1','Widget',5000),('P2','Gadget',3000),('P3','Gizmo',800)"]

query
  SELECT p.id, p.name, p.revenue, kv.value AS note
  FROM smoke_products p
  LEFT JOIN _hyperdb_kv_store kv
    ON kv.store_name = 'product_notes' AND kv.key = p.id
  ORDER BY p.id
```

Expected: P1 and P3 carry their notes; **P2 survives with `note: null`**
(the `LEFT` join keeps unannotated rows).

---

## 10. Table is hidden but accessible

```
describe                    → table list does NOT include _hyperdb_kv_store
query SELECT COUNT(*) FROM _hyperdb_kv_store   → succeeds (directly queryable)
```

Hidden-but-accessible, exactly like `_hyperdb_saved_queries`.

---

## 11. Concurrency / atomicity (optional, deeper)

The backing table has **no index**; uniqueness on overwrite and
single-serve on pop rely on the engine serializing writes within one server
process. To stress this against a live server, fan out concurrent calls
(e.g. from a script or a fleet of parallel tool calls) to a scratch store
named `smoke_concurrency` (keep it local — omit `database` — and purge
it in §12):

- **N concurrent `kv_set` to the same key** → the store ends with exactly
  **one** row for that key (no duplicates in the indexless table).
- **M concurrent `kv_set` to distinct keys** → exactly M rows, none lost.
- **P concurrent `kv_pop` draining the store** → every found key is
  returned **at most once** (no double-serve); surplus poppers get
  `{"found": false}`.

This validates the "atomic within a single server process" guarantee
documented on `kv_pop` and the `hyper://schema/kv` resource. (Cross-process
writes to a shared persistent store via the daemon are **not** guarded by a
DB constraint — that limitation is documented, not a smoke-test failure.)

---

## 12. Cleanup + verification

Purge every scratch store and table, then confirm the databases are back to
their starting state:

```
kv_clear store=smoke
kv_clear store=smoke_routing
kv_clear store=smoke_routing database=persistent
kv_clear store=smoke_concurrency        # only if you ran §11
kv_clear store=product_notes
execute ["DROP TABLE IF EXISTS smoke_products"]

# verify nothing of ours remains:
kv_list_stores                    → {"count": 0, "stores": [], "resolved_database": "local"}   # (or only pre-existing non-smoke stores)
kv_list_stores database=persistent → no smoke_* / product_notes stores
describe database=persistent       → only the real, pre-existing tables (no smoke_*)
```

If the persistent database shows any `smoke_`/`product_notes` remnant, the
run left debris — clear it before finishing.

---

## Expanding this doc

Add a numbered section per new capability or regression you want covered.
Keep the format: the exact tool calls, the `→` expected JSON, and one line
on *why* the check matters. When a smoke check hardens into something CI
should enforce, promote it into `tests/kv_tools_tests.rs` (or the relevant
suite) and note the automated equivalent here.
