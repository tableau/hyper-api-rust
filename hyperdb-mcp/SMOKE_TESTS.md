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

- the **ephemeral** primary (the default target; a fresh temp `.hyper` that
  is deleted on server restart), and
- the **persistent** database (`database: "persistent"` / `persist: true`)
  which is the user's durable workspace and **may already hold real data**.

**Rules for smoke testing:**

1. **Default to the ephemeral store.** Omit `database` on every call unless
   you are explicitly testing routing. Ephemeral writes cost nothing and
   vanish on restart.
2. **Never create, drop, or overwrite a table without checking first —
   scoped to the database you're about to write to.** A real `products`
   table with a thousand rows can already be sitting in the persistent DB.
   Before any `CREATE`/`DROP`, confirm the name is free *in that database*:
   run `describe table=<name> database=persistent` (or a
   `SELECT COUNT(*) FROM <name>` via `query database=persistent`) when the
   target is persistent — a bare `describe` inspects only the **ephemeral**
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

- `hyperd` available (`HYPERD_PATH` set, or on `PATH`).
- The `hyperdb` MCP tools connected and responding.
- Confirm the server is up and note its mode before you start:

```
status
```

Expected: `{"hyperd_running": true, ..., "read_only": false, "engine": {"mode": "daemon"|"local", ...}}`.
Note `read_only` — if `true`, the four KV **mutators** (`kv_set`,
`kv_delete`, `kv_pop`, `kv_clear`) are expected to be **rejected** (see
§7); the four readers still work.

Throughout, `→` shows the expected JSON the tool returns. Store/key names
below all begin with `smoke` so they're easy to spot and purge.

---

## 1. Server + KV surface present

The server should expose 8 `kv_*` tools and the `hyper://schema/kv`
resource.

- `kv_*` tools: `kv_set`, `kv_get`, `kv_delete`, `kv_list`,
  `kv_list_stores`, `kv_size`, `kv_pop`, `kv_clear`.
- Reading `hyper://schema/kv` returns text mentioning `_hyperdb_kv_store`
  and a `LEFT JOIN` template.

---

## 2. Create / read / overwrite (upsert)

```
kv_set   store=smoke key=greeting value="hello world"     → {"stored": true, "store": "smoke", "key": "greeting"}
kv_get   store=smoke key=greeting                          → {"found": true, "value": "hello world"}
kv_get   store=smoke key=does_not_exist                    → {"found": false, "value": null}
```

A miss is **not** an error — `found: false` with a `null` value.

**Overwrite must not create a duplicate row** (the backing table is
indexless; `kv_set` is an app-side upsert):

```
kv_size  store=smoke                                       → {"store": "smoke", "size": 1}
kv_set   store=smoke key=greeting value="HELLO AGAIN"      → {"stored": true, ...}
kv_size  store=smoke                                       → {"store": "smoke", "size": 1}   # still 1, not 2
kv_get   store=smoke key=greeting                          → {"found": true, "value": "HELLO AGAIN"}
```

---

## 3. Listing, size, store discovery

Seed a few keys, then list:

```
kv_set store=smoke key=alpha   value=1
kv_set store=smoke key=bravo   value=2
kv_set store=smoke key=charlie value=3

kv_list        store=smoke   → {"store": "smoke", "count": 4, "keys": ["alpha","bravo","charlie","greeting"]}   # sorted ascending
kv_size        store=smoke   → {"store": "smoke", "size": 4}
kv_list_stores               → {"count": 1, "stores": ["smoke"]}
```

`kv_list` keys are always sorted ascending. `kv_list_stores` reflects only
stores that currently hold rows (there is no separate store registry — an
emptied store disappears from the list; see §5).

---

## 4. Value fidelity — JSON, empty, large

```
kv_set store=smoke key=config    value='{"retries": 3, "nested": {"flag": true}}'
kv_get store=smoke key=config    → {"found": true, "value": "{\"retries\": 3, \"nested\": {\"flag\": true}}"}   # byte-for-byte

kv_set store=smoke key=empty_val value=""
kv_get store=smoke key=empty_val → {"found": true, "value": ""}    # empty string, NOT a miss

kv_set store=smoke key=big_blob  value="<a few hundred+ chars>"
kv_get store=smoke key=big_blob  → {"found": true, "value": "<same string, intact>"}
```

The empty-string case is the important one: `{"found": true, "value": ""}`
must stay distinct from a miss `{"found": false, "value": null}`.

---

## 5. Destructive semantics — delete, pop, clear

**Delete is idempotent and reports whether the key existed:**

```
kv_delete store=smoke key=greeting        → {"deleted": true,  ...}   # existed
kv_delete store=smoke key=greeting        → {"deleted": false, ...}   # already gone — no error
kv_delete store=smoke key=never_existed   → {"deleted": false, ...}
```

**`kv_pop` destructively removes the lowest-keyed entry** (a work-queue
drain in ascending key order):

```
# with keys [alpha, bravo, charlie, config, empty_val, big_blob] present
kv_pop store=smoke   → {"found": true, "key": "alpha",    "value": "1"}
kv_pop store=smoke   → {"found": true, "key": "big_blob", "value": "..."}   # 'b' < 'c'
kv_pop store=smoke   → {"found": true, "key": "bravo",    "value": "2"}
```

**`kv_clear` empties the store and returns the count removed:**

```
kv_size  store=smoke   → {"store": "smoke", "size": N}
kv_clear store=smoke   → {"store": "smoke", "removed": N}
kv_size  store=smoke   → {"store": "smoke", "size": 0}
```

**Empty-store edge cases:**

```
kv_pop   store=smoke   → {"found": false}          # nothing to pop
kv_clear store=smoke   → {"store": "smoke", "removed": 0}   # idempotent
kv_list_stores         → {"count": 0, "stores": []}   # emptied store drops out
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
kv_get store=smoke key=k    → {"found": ...}
kv_list store=smoke         → {...}
kv_size store=smoke         → {...}
kv_list_stores              → {...}

# mutators are blocked:
kv_set    store=smoke key=k value=v  → error READ_ONLY_VIOLATION ("... not permitted in read-only mode")
kv_delete store=smoke key=k          → error READ_ONLY_VIOLATION
kv_pop    store=smoke                 → error READ_ONLY_VIOLATION
kv_clear  store=smoke                 → error READ_ONLY_VIOLATION
```

---

## 8. Database routing + isolation

**⚠ Touches the persistent database. Clean up after (§12).**

Each database keeps its own isolated set of stores. The same store name in
two databases holds independent values. `persist: true` and
`database: "persistent"` target the same place.

```
kv_set store=smoke_routing key=where  value="ephemeral"                       # → ephemeral (default)
kv_set store=smoke_routing key=where  value="persistent" database=persistent  # → persistent
kv_set store=smoke_routing key=where2 value="via-flag"    persist=true        # → persistent (same DB)

kv_get  store=smoke_routing key=where                       → {"found": true, "value": "ephemeral"}
kv_get  store=smoke_routing key=where  database=persistent  → {"found": true, "value": "persistent"}
kv_get  store=smoke_routing key=where2 persist=true         → {"found": true, "value": "via-flag"}

kv_list store=smoke_routing                       → {"store": "smoke_routing", "count": 1, "keys": ["where"]}            # ephemeral
kv_list store=smoke_routing database=persistent   → {"store": "smoke_routing", "count": 2, "keys": ["where","where2"]}   # persistent
```

The ephemeral and persistent `where` values differ → isolation holds.
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
join. **Run this in the ephemeral DB** (create a `smoke_`-prefixed table):

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
named `smoke_concurrency` (keep it ephemeral — omit `database` — and purge
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
kv_list_stores                    → {"count": 0, "stores": []}   # (or only pre-existing non-smoke stores)
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
