# hyperdb-mcp Roadmap

Forward-looking design sketches for features that aren't bugs or tech debt but are worth keeping on the radar. Ordered roughly by expected value vs. implementation effort.

For the current codebase (architecture, design decisions, how to add a tool, known tech debt) see [DEVELOPMENT.md](DEVELOPMENT.md). This file is the inverse: things that **don't exist yet** but we'd like to think about before starting.

Each section follows a loose template: Motivation → Architecture sketch → Estimated size → Risks / open questions → Verdict.

---

## Cross-database tools

First-class tools for attaching additional `.hyper` databases and
landing query results into tables. The registry lives on
`HyperMcpServer` and is replayed against a fresh `Engine` whenever
`with_engine` recovers from a ConnectionLost error, so attachments
survive hyperd crashes transparently.

- `attach_database(alias, kind, path, writable?)` — attach a `.hyper`
  file under a chosen alias. `kind="local_file"` is the only kind
  supported today; `"tcp"` (remote hyperd) and `"grpc"` (Data 360)
  are planned. `writable` defaults to `false`; the server's
  `--read-only` flag always wins. The alias `"local"` is reserved for
  the primary workspace.
- `detach_database(alias)` — drops the alias from the registry and
  from the current connection. No-op when the alias is unknown.
- `list_attached_databases()` — enumerates every live attachment
  with its kind, source, writable flag, attach time, and
  (best-effort) a count of visible `public`-schema tables.
- `copy_query(sql, target_table, mode, target_database?, temp_attach?)`
  — runs a read-only SELECT and lands the rows into a target.
  `mode` is explicit: `"create"` errors if the target exists,
  `"append"` errors if it doesn't, `"replace"` drops and recreates.
  `target_database` defaults to the primary workspace; any other
  alias must be attached with `writable: true`. `temp_attach` is
  detached automatically even if the query fails.

Example — JOIN across a scratch `.hyper` file and the primary
workspace, then land the result:

```
attach_database(alias="src", kind="local_file", path="/tmp/scratch.hyper")
copy_query(
  sql="SELECT s.id, s.name, p.amount FROM src.public.customers s JOIN orders p ON s.id = p.customer_id",
  target_table="enriched_orders",
  mode="create"
)
detach_database(alias="src")
```

`_table_catalog` is stamped with `load_tool = "copy_query"` and the
serialized request when the destination is the primary workspace;
attached destinations aren't tracked (their catalog isn't ours).

Rough edges for now: `describe` and the `hyper://tables` resource
still only enumerate tables in the primary workspace, not attached
databases. Use `SELECT * FROM {alias}.pg_catalog.pg_tables WHERE
schemaname = 'public'` as an interim workaround.

---

## Raw fallbacks for cross-workspace data movement

When the four tools above don't cover the use case (other MCP server
owns the file you want to read, remote host, non-Hyper consumer),
keep these workarounds in mind:

1. **`.hyper` → `.hyper` export then load.** Single-table transfer.
   Fastest because `.hyper` is Hyper's native format.
   ```
   # in HyperDB (sandbox)
   export(table="scratch_data", path="/tmp/scratch.hyper", format="hyper")
   # then in HyperDB-persistent
   load_file(table="scratch_data", path="/tmp/scratch.hyper")
   ```
2. **CSV / Parquet / Arrow IPC roundtrip.** Universal fallback —
   works between any two workspaces and between HyperDB and
   non-Hyper consumers. Pays the serialization cost both ways.

---

## `switch_workspace` mid-session tool

Conceptually clean: a `switch_workspace(path)` MCP tool that tears down the current Engine and re-instantiates against a different `.hyper` file without a process restart. Also resets the saved-queries store (back to ephemeral/persistent pick), subscription registry, and active watchers.

Useful if you'd rather "flip between N workspaces in one chat" than "have N MCP servers in the sidebar". Feasible as ~100 LOC in `server.rs` but introduces subtleties: what happens to in-flight subscriptions, watcher threads with state, and saved queries whose results reference the old workspace's tables. Probably gated behind a CLI flag so it's opt-in.

Scratched for now — the two-MCP-server pattern (`HyperDB` + `HyperDB-persistent`) covers this use case without adding a new failure mode.

---

## Catalog awareness for attached databases (follow-up to cross-database tools)

The [Cross-database tools](#cross-database-tools) shipped the core
ATTACH / DETACH / copy surface. The remaining piece is teaching the
catalog views about attached databases so the LLM can discover
tables without issuing raw `pg_catalog` SQL:

- `describe` grows an optional `database` parameter to enumerate
  tables under a specific alias (default: primary only).
- `hyper://databases/{alias}/tables` resource — per-attachment
  schema catalog that mirrors the existing `hyper://tables` shape.
- `list_attached_databases()`'s `tables_visible` grows from a count
  into a full name list (cheap because attachments are small and
  queries will rerun `pg_catalog` anyway).

Also punts: remote kinds (`"tcp"` / `"grpc"`) on `attach_database`.
Those need credential-profile infrastructure (auth tokens, key
material) that doesn't exist yet — the shared-daemon work landed
already, but it covers only the local-`hyperd` case.
