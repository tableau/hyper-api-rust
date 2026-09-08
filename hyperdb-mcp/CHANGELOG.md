# Changelog

All notable changes to the `hyperdb-mcp` crate will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.0.0-rc.4] - 2026-09-08

### Changed

- **The daemon now connects to its `hyperd` engine over a local IPC channel
  instead of TCP** — a Unix domain socket on Unix/macOS, a named pipe on
  Windows. Only the daemon's engine connection moves: the health/control
  channel and daemon discovery keep their existing loopback TCP port, so
  `daemon.json` still carries a numeric `health_port`. What changes there is
  `hyperd_endpoint` — a daemon-mode session now advertises a socket path (e.g.
  `~/.hyperdb/sockets/hyper`) rather than `127.0.0.1:<port>`, and `status`
  reports it as `transport: "unix_domain_socket"` (or `"named_pipe"`). The
  socket directory is created private to the owner (`0700`) before `hyperd`
  binds in it, as ordinary hygiene for a path under the state directory. A
  local `--no-daemon` session is unchanged and stays TCP.

## [1.0.0-rc.3] - 2026-09-07

### Added

- **`export` — `schema_fidelity` in the response for `format="hyper"`.** Reports
  `fully_preserved` plus per-class counts (`not_null_columns`,
  `default_columns`, `collated_columns`, `assumed_primary_keys`,
  `assumed_unique_constraints`) and an `unpreserved` list naming each entry's
  table and column, the reason, and the offending expression. The table matters
  because a whole-database export merges one report per table, and a bare
  column name cannot be acted on. Hyper stores non-literal defaults
  database-qualified — `NOW()`
  reads back as `"mydb"."pg_catalog"."now"()` — so re-emitting one into the
  exported file would leave it depending on the source database still being
  attached. Those defaults are dropped and listed rather than reproduced
  unsoundly, so a partially faithful backup says so at export time instead of
  looking clean.
- **`status` — `engine.connection` block**
  ([#124](https://github.com/tableau/hyper-api-rust/issues/124)) — reports the
  `hyperd` endpoint in the forms another Hyper client needs: `transport`
  (`tcp` / `unix_domain_socket` / `named_pipe` / `unknown`), `host` and `port`
  (TCP only, otherwise `null`), `socket_path` (the IPC transports only,
  otherwise `null`), and `connection_descriptor` — the scheme-qualified string
  `hyperd` emits over its callback connection (`tab.tcp://host:port`,
  `tab.domain://<dir>/domain/<name>`, `tab.pipe://<host>/pipe/<name>`). That
  descriptor is the only spelling the Hyper API family accepts: verified
  against `tableauhyperapi` 0.0.26479, `tab.tcp://127.0.0.1:<port>` connects
  while both `tcp:127.0.0.1:<port>` and the bare `127.0.0.1:<port>` already in
  `hyperd_endpoint` are rejected with *"The connection string must be of the
  form `<scheme>://<rest>`"*. Transport is recovered from the endpoint's shape
  rather than assumed, so an IPC session reports `host: null` / `port: null`
  instead of a fabricated `host:port`, and an unrecognized endpoint is
  `unknown` with a `null` descriptor. Present on the full and the
  `engine_busy` degraded response alike (`null` there when no endpoint is
  known yet). Every session is TCP on every platform today — the daemon sets
  `TransportMode::Tcp` explicitly and `HyperProcess` defaults to it.
- **`get_readme` — `QUALIFY` and `APPROX_COUNT_DISTINCT` dialect notes**
  ([#164](https://github.com/tableau/hyper-api-rust/issues/164)) — the SQL
  quick-reference now records that `QUALIFY` is absent from the grammar
  (SQLSTATE `42601`, a syntax error, as distinct from `42883` for a missing
  function) and gives the equivalent subquery/CTE rewrite, and that
  `APPROX_COUNT_DISTINCT` accelerates the distinct step only, so a per-row
  string concatenation in its argument is paid either way and can hide the
  speedup.
- **`kv_set_many` tool** — atomic batch write accepting an array of
  `{key, value}` entries. Validates all keys before opening the transaction; an
  invalid key aborts the whole batch without writing anything. Default behavior
  (`overwrite` absent or `true`) reports
  `{stored, created, overwritten, total_bytes}`; with `overwrite: false`,
  existing keys are skipped (not errors) and the response reports
  `{stored, created, skipped, total_bytes}` where `created` is the number of
  keys newly inserted. `total_bytes` sums all submitted values, so it is an
  upper bound on bytes actually persisted when keys are skipped or duplicated.
  Each oversized entry (> 1 MiB) adds a keyed `warning` to a `warnings` array.
- **`kv_set` — `value_path` parameter** — absolute path to a file whose
  contents become the value (read server-side). Provide exactly one of `value`
  or `value_path`; neither or both is `INVALID_ARGUMENT`. Reads any path the
  server process can read (same posture as `load_file` — no sandbox), with I/O
  errors preserved (`PermissionDenied` → `ErrorCode::PermissionDenied`, not
  collapsed to `FileNotFound`). A hard 64 MiB size cap is enforced against the
  file's metadata *before* reading, so a stray path to a huge file is rejected
  with `INVALID_ARGUMENT` instead of being slurped into memory.
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
- **Side-effect-free native `hyperdb-mcp doctor` diagnostics.** Human and
  JSON reports compare authoritative native MCP/Rust API identity, bounded
  launcher-reported npm provenance, resolved configuration, verified daemon
  identity, and the measured MCP catalog without creating directories,
  starting Hyper, or opening a database.
- **Canonical `resolved_database` result metadata.** Successful responses
  from all 21 database-routed tools now identify the effective `local`,
  `persistent`, or lowercase attached alias after routing precedence;
  `copy_query` also retains `target_database`.
- **Diagnostic chart presentation controls.** MCP `chart` adds
  `bar_orientation`, `label_values`, `show_legend`, and positive-only
  `y_scale`, while preserving the public Rust `ChartOptions` surface and
  existing rendering defaults.
- **`daemon::state_perms` module, with `state_perms::ensure_owner_only_dir`.**
  New public surface on the library target: the single place that decides how
  the daemon's state directory and the files in it are created and tightened
  (see Security, below). It is public because `hyperdb-mcp`'s `[[bin]]` is a
  separate crate to Cargo and sets up the daemon's log directory itself;
  `pub(crate)` would not reach it. Like the rest of `daemon::*` it is plumbing
  for the binary rather than an API to build on — the library target "is not a
  documented API surface" — so it may be narrowed without a breaking change.

### Changed

- **The Rust library target is not a supported API surface, and its 21 modules
  are now `#[doc(hidden)]`.** `src/lib.rs` already carried a lint `reason`
  saying the library "is not a documented API surface", while this crate's
  `README.md` promised, without scope, that "the public API is stable and
  follows semantic versioning". Against a crate where every module is `pub`,
  that sentence promised semver stability on all 21 modules and every item in
  them, which is why two incidental internals had to be written up as API
  events: `DaemonState` gaining a private field ([#289]) and `state_perms`
  becoming new public surface ([#295]). The README now scopes the promise to
  the **MCP tool surface** — tool names, their parameters, and their behavior
  as reached over the MCP protocol — and states that the library target is
  excluded; the root `README.md` crate table says the same, alongside the
  equivalent note that already existed for `hyperdb-api-core`.

  **Not marked BREAKING, and nothing was privatised.** `pub(crate)` is not
  available for any of the 21 modules: Cargo compiles the `hyperdb-mcp`
  `[[bin]]`, each file under `tests/`, and `examples/demo.rs` as separate
  crates that can only reach library items through the external
  `hyperdb_mcp::` path, and every module is used by at least one of them
  (`paths` by the binary alone, `stats`, `subscriptions` and `watcher` by one
  test file each). So `pub` is load-bearing for compilation, not an API
  commitment. `#[doc(hidden)]` removes the modules from published rustdoc and
  signals intent; it does not affect name resolution, so no code that compiled
  before stops compiling. Narrowing a module to `pub(crate)` later would be
  source-breaking and would carry the marker — this does not.

[#289]: https://github.com/tableau/hyper-api-rust/pull/289
[#295]: https://github.com/tableau/hyper-api-rust/pull/295

<!-- The two entries below are retroactive. Both changes shipped in 0.7.3
     (commit 44bdf1e, PR #243) and were missing from this file; this crate's
     `## [Unreleased]` section has not been rolled over since 0.5.0, so the
     version each landed in is stated inline rather than implied by a heading.
     See issue #276. -->

- **BREAKING (landed in 0.7.3):
  `daemon::health::report_hyperd_error_to_daemon` changed from `fn()` to
  `fn(health_port: u16)`.** The behavior change was correct — the old body
  reported to `discovery::resolve_port().base`, the *configured* base port,
  which is the wrong target whenever the daemon scanned past its base to find
  a free one, so the report went to nothing or to a foreign listener and the
  daemon never learned its `hyperd` had died. Taking the port from the
  discovered daemon is the fix, and
  `report_hyperd_error_targets_discovered_health_port` pins it. What was
  missing is the record: the entry that shipped sat under `### Fixed`, named
  no function, and carried no breaking marker, so nothing warned a downstream
  library consumer that `0.7.2 → 0.7.3` — a compatible update under `^0.7.2`
  — required a source change. **Callers pass the health port of the daemon
  they discovered**, e.g. `discovery::discover().map(|info| info.health_port)`,
  rather than relying on the function to resolve it.
- **BREAKING (landed in 0.7.3): `daemon::health::send_command_with_timeout`
  kept its signature but changed what two of its arguments mean.** Nothing a
  compiler can catch, which is why it needs to be written down:
  - `read_timeout` was a **per-read** timeout — set once as `SO_RCVTIMEO`, so
    each underlying `read` syscall inside a `BufReader::read_line` got the
    full duration, and the write phase was not bounded at all. It is now an
    **absolute deadline** covering the whole write-and-read exchange. A caller
    passing `Duration::from_millis(200)` for a multi-read exchange used to get
    200 ms per read and now gets 200 ms in total. **Callers should size this
    as a budget for the entire round trip**, not per read, and can now rely on
    the write phase being bounded too, which it previously was not.
  - Responses are now capped at **64 KiB**, returning
    `io::ErrorKind::InvalidData` rather than growing without limit while
    waiting for a newline.

  Both were undocumented; `AGENTS.md` reminder 8 requires a per-crate entry
  for any change to a publishable crate's public API surface.
- **Histograms now honor an explicit `x_range`, and exclude the values that
  fall outside it.** `x_range` was validated for every chart type but read
  by line/scatter only. Bar charts are documented as ignoring it
  (unchanged); the undocumented case was histogram, whose doc promised "all
  frames/charts share the same x extent" while the renderer silently binned
  the data's own min/max regardless. `draw_histogram` now uses an explicit
  `x_range` as the bin extent.

  **This changes the rendered image for a caller already passing `x_range`
  to a histogram** — previously that argument had no effect at all, so any
  such call now produces a different chart. Values outside the range are
  dropped from the bins rather than folded into the first/last bin: folding
  would credit a bin with observations that don't belong to it (100 uniform
  values over 0..9 with `x_range = [4, 6]` and `bins: 2` would give bin
  `[4, 5)` 50 observations against a true 10, a 5× overstatement that reads
  as a real cluster). This is *not* the same as line/scatter, deliberately —
  their `apply_ranges` only sets axis bounds, so plotters clips
  out-of-range geometry in place and a clipped point becomes invisible
  rather than being relocated. Excluding is the histogram equivalent.

  `rows_plotted` therefore counts only the values actually binned, so it no
  longer implies more in-window observations than the chart shows, and the
  `chart` tool reports `excluded_out_of_range` in its stats block when any
  value was dropped (`ChartResult::excluded_out_of_range` on the Rust side).
  A range containing no data renders an empty histogram at the requested
  extent rather than erroring, so a per-window animation frame stays
  renderable. `ChartOptions::x_range`'s rustdoc and the `get_readme` payload
  now both document the per-chart-type behavior; previously neither
  mentioned histograms. Part of
  [issue #277](https://github.com/tableau/hyper-api-rust/issues/277).
- The LLM-facing SQL-dialect notes — the `get_readme` payload and the MCP
  server instructions — document three verified engine behaviors that were
  previously absent. `to_char` accepts DATE / TIMESTAMP (`to_char(DATE
  '2020-01-02', 'YYYY')` → `2020`) but has **no numeric overload**:
  `to_char(123.456, 'FM990.00')` fails with `42601 unsupported data types in
  call to 'to_char'`. That is an argument-type error, not a missing function —
  a function Hyper genuinely lacks returns `42883`, as `format()` does.
  `expr::TEXT` is the scale-preserving numeric-output idiom
  (`CAST(9.5 AS NUMERIC(8,2))::TEXT` → `9.50`) and the practical stand-in for
  numeric `to_char`. Parquet columns stored with the physical `NullType`
  cannot be read at all; see the matching entry under **Fixed**. Part of
  [issue #165](https://github.com/tableau/hyper-api-rust/issues/165).
- The `arrow` and `parquet` dependencies moved from **58** to **59**. Not a
  library-API change (this crate ships a binary), but it removes the `thrift`
  dependency and the Apache Thrift excessive-size-allocation advisory with it:
  `parquet` 58.x pinned `thrift ^0.17`, and `parquet` 59 dropped thrift
  entirely.

- **BREAKING:** `Engine::execute_in_transaction` now holds `hyperdb-api`'s RAII
  `Transaction` guard instead of driving the session with the unguarded
  `begin`/`commit`/`rollback` methods
  ([#72](https://github.com/tableau/hyper-api-rust/issues/72)). The helper
  takes `&mut self` and its closure receives an `EngineTransaction` view
  rather than `&Engine`, so transactional work can no longer reach around the
  transaction to the raw connection. Rollback on the panic path is now the
  guard's `Drop` rather than a hand-written `catch_unwind`; commit and
  error-path rollback are unchanged, including the `tracing::warn!` on a
  failed rollback. The panic itself still propagates, exactly as the previous
  `resume_unwind` did, so a panicking tool call still poisons the server's
  engine mutex: the guard cleans up the SQL session, not the process state. The ingest entry points (`ingest_json`, `ingest_csv`,
  `ingest_csv_file`, `ingest_json_file`, `ingest_parquet_file`,
  `ingest_arrow_ipc_file`, `ingest_iceberg_table`) and
  `merge_via_temp_table` take `&mut Engine` to match; `merge_via_temp_table`
  passes the engine to its `replace_load` closure instead of having callers
  capture it.
- **New** `Engine::with_search_path(alias, f)` — the closure form of
  `scoped_search_path`, for callers that need `&mut Engine` inside the scope.
  It restores the primary database on the `Ok`, `Err`, and panic paths, the
  same as the guard's `Drop`. `scoped_search_path` is unchanged and still
  preferred where `&Engine` suffices.
- **BREAKING:** the minimum supported Rust version is now **1.88**, up from
  1.81, and the crate is compiled with **edition 2024**. 1.88 is the version
  Red Hat Enterprise Linux 9.7 ships as `rust-toolset`.
- **The daemon's health listener no longer polls for connections.** The accept
  loop slept 5 ms between attempts on a non-blocking listening socket, so that
  `DaemonState::should_shutdown` could be re-checked between accepts. That put
  the whole cost of shutdown responsiveness on an idle daemon, and made every
  connection pay a share of the sleep before it was even read. The loop now
  waits for the socket to become *readable* instead of sleeping on a timer, so
  an arriving connection is accepted with no added latency, and
  `request_shutdown` returns that wait immediately with a throwaway loopback
  connection; the loop re-checks the flag on every pass and drops the wake
  connection unread. Measured on an Apple M3 Max over a 10 s idle window,
  release build, with `getrusage(RUSAGE_SELF)` around a real listener thread:
  context switches fell from **1736 (174/s) to 1**, idle CPU from
  **16 985 µs to 35 µs**, PING round-trip median from **4670 µs to 659 µs**,
  and shutdown latency from **5385 µs to 456 µs** — so both halves of the
  original trade improved rather than one being chosen over the other. `EINTR`
  from the accept loop is now handled as a benign retry instead of being
  logged as an accept error and penalized with a 500 ms sleep
  ([#274](https://github.com/tableau/hyper-api-rust/issues/274)).

  **The readiness wait still times out, once per second, and that cadence is a
  deliberate correctness floor rather than a leftover.** The wake is what makes
  shutdown prompt, but it can be lost — an unregistered wake port, a second
  listener sharing one `DaemonState`, a connect that never lands — and since
  every `request_shutdown` caller in the tree follows it with a `join()`, a
  purely wake-driven loop turns any lost wake into a permanent hang rather than
  a slow shutdown. One second bounds that unconditionally while still costing
  ~1 wakeup/s against the ~174/s the 5 ms sleep cost. Windows keeps the 5 ms
  interval, because the readiness wait there is a plain sleep that an arriving
  connection cannot cut short, so the interval doubles as the accept latency.

  **The idle cost is now reproducible rather than asserted.**
  `tests/health_idle_cost_tests.rs` holds the `getrusage` measurement behind
  `#[ignore]`, and doubles as a regression guard that fails if a busy poll ever
  returns. It reports 5 s of idle costing **8 context switches (1.6/s) and
  231 µs of CPU**, against **962 (192.4/s) and 21.3 ms** for the same harness
  at the old 5 ms cadence. Run it with
  `cargo test -p hyperdb-mcp --test health_idle_cost_tests --release -- --ignored --nocapture`.

  **`DaemonState`'s fields are now private** — `last_activity`, `shutdown` and
  `restart_requested` alongside the new `wake_port` — so the struct can no
  longer be built with a struct literal or mutated field-by-field from outside
  the crate. `DaemonState::new()`, `Default`, and the existing accessor pairs
  (`touch`/`idle_duration`, `request_shutdown`/`should_shutdown`,
  `request_restart`/`consume_restart_request`) are unaffected and are what
  every in-tree caller already uses. `shutdown` is the one that mattered:
  `state.shutdown.store(true, ...)` is what a public field invites, it reads
  back through `should_shutdown()` exactly like `request_shutdown()`, and it
  skips the wake — so it left the listener waiting out a full interval instead
  of returning at once.
- **KV attachment/read-only clarification (supersedes the shorthand in the
  Added notes above).** The global `--read-only` guard leaves the four KV
  readers available, but every `kv_*` call targeting a user attachment still
  requires that attachment to be registered writable because the backing table
  may need initialization.
- **Status now has one full/degraded identity contract.** Both shapes report
  correct MCP/Rust API installation identity, `default_database: "local"`,
  attachments, watchers, and read-only state. `engine_busy: true` explicitly
  means partial, inconclusive statistics that callers should retry.
- **Chart is documented and bounded as a quick SQL-to-image diagnostic.** Its
  inline/file delivery, proportional temporal x axes, categorical override,
  presentation controls, and finite/log range requirements are now consistent
  across the MCP schema, smoke/demo guidance, the public README, and
  `get_readme`.

### Fixed

- **On Windows, the daemon state directory now resolves from `%USERPROFILE%`
  before `HOME`.** `discovery::state_dir` documented `~` as "`HOME` on Unix,
  `USERPROFILE` on Windows" but `home_dir()` tried `HOME` first on every
  platform. MSYS2, Cygwin and Git Bash routinely set `HOME` to a path of their
  own outside the user profile, so under those shells the state directory
  landed outside `%USERPROFILE%` and did not inherit its ACL — the only thing
  restricting these files on Windows — without the user having set
  `HYPERDB_STATE_DIR` or otherwise asked for it. Windows now prefers
  `USERPROFILE`, keeps `HOME` as a last resort so a machine that resolved
  before still resolves, and matches `paths::persistent_home_dir`. Unix
  resolution is unchanged. **Behaviour change on Windows:** a shell that sets
  both to different paths now gets the profile-relative state directory, so a
  daemon started before this change may not be discovered by a client started
  after it until the old one is stopped.
- **Daemon port `0` is now rejected at both entry points, instead of quietly
  multiplying daemons.** `--port` promises an exact bind and
  `"0".parse::<u16>()` succeeds, so `0` passed validation at both the flag and
  `HYPERDB_DAEMON_PORT` and reached `TcpListener::bind`, which assigns an
  *ephemeral* port rather than port 0. The failure then compounded rather than
  surfacing: with the resulting `PortScan { base: 0, span: 1 }`, every client's
  scan probed port 0, got a connection error, and read `ProbeResult::Refused`
  as "this port is free" — so each client that missed the discovery fast path
  concluded no daemon existed and started another daemon-and-`hyperd` pair, on
  another ephemeral port no scan could find. The pairs accumulated silently.
  `--port` now carries a `1..` range (clap reports a usage error, exit 2), and
  the `HYPERDB_DAEMON_PORT` chain filters `0` into the same default fallback
  that unparseable values already take. Part of
  [issue #275](https://github.com/tableau/hyper-api-rust/issues/275).

  **`--port 0` was previously accepted and is now a usage error.** It never
  did what the help text promised, so nothing can have depended on it
  deliberately, but a script passing `0` will now fail loudly rather than
  leaking daemons.
- **`export(format="hyper")` silently dropped every column constraint.** The
  copy ran on `CREATE TABLE AS SELECT`, which infers the destination schema
  from the query's result columns — types but no constraints — so a database
  with `NOT NULL` columns produced a "backup" in which every column was
  nullable, with no defaults and no keys. Data was intact; only the schema was
  quietly relaxed, which is the kind of loss a user discovers months later.
  Each table now goes through `hyperdb-api`'s constraint-preserving
  `Catalog::copy_table`, so `NOT NULL`, `DEFAULT`, `COLLATE`, `ASSUMED PRIMARY
  KEY` and `ASSUMED UNIQUE` all survive an export and a subsequent re-attach of
  the exported file. Fixes
  [issue #127](https://github.com/tableau/hyper-api-rust/issues/127).
- `export(format="hyper")` reported `rows: 0` regardless of how much it
  copied, because `CREATE TABLE AS SELECT` reports no affected rows. The copy
  now populates via `INSERT ... SELECT` and reports the count it actually
  wrote.
- Parquet and Arrow IPC files carrying a column stored with the physical
  `NullType` — what a writer emits for an optional column that happens to be
  entirely null in one partition — are rejected during footer inspection, with
  an error naming the offending column(s), instead of being handed to the
  engine. `arrow_type_to_hyper` mapped `DataType::Null` to `TEXT`, so
  `inspect_file` advertised a readable `TEXT` column with `null_count: 0`, and
  `load_file` / `load_files` / `query_file` (and the directory watcher) built
  well-formed-looking SQL only for hyperd to answer `42804 ... hinting at a
  corrupted file` — sending users to audit a file that is intact. There is no
  SQL-level workaround: selecting only the other columns fails identically,
  and a `schema` override becomes a cast in the projection that the engine
  never evaluates. The new error states the only real remedy — re-type the
  column where the file is written, e.g. cast it to `DOUBLE`. `inspect_file`
  now reports such a column as type `NULL`, which `map_hyper_type`
  deliberately does not resolve, so an override copied from that report is
  rejected rather than silently becoming `TEXT`. Part of
  [issue #165](https://github.com/tableau/hyper-api-rust/issues/165).
- **Behavior change for consumers parsing NUMERIC query results.** A NUMERIC
  value that an `f64` cannot represent is now emitted as its exact decimal
  **string** rather than a silently rounded JSON number, so a field that
  previously always arrived as a number can now arrive as a string. Values an
  `f64` *can* represent are unaffected and keep their JSON-number shape —
  including two-decimal money values such as `9.50` → `9.5`, and every value
  of 15 significant digits or fewer — so only results that were already wrong
  change shape. `CAST('99999999999999999.99' AS NUMERIC(19,2))` was emitted as
  `1e17`: `Numeric::to_string()` is exact, but the `parse::<f64>()` that
  followed discarded the low digits, and the exact-string fallback never fired
  because `1e17` is a perfectly finite `f64`. Loss is now detected by
  round-tripping the `f64` back to a decimal at the value's own scale and
  comparing `Numeric` values, which is deliberately *not* a textual
  comparison: `9.50` stringifies as `"9.50"` but formats back from `f64` as
  `"9.5"`, so a string rule would have pushed ordinary money values into
  strings. Round-tripping is also exactly as tight as `f64` really is, rather
  than approximating it with a significant-digit count — `2^53`
  (`9007199254740992`, 16 digits) is exactly representable and stays a number,
  while its neighbour `9007199254740993` was previously returned as
  `9007199254740992` with nothing in the output to suggest a rounding had
  occurred, and is now the exact string. Chart rendering is unchanged; it
  already carried an `f64` coordinate and exact display text side by side.
  Part of [issue #165](https://github.com/tableau/hyper-api-rust/issues/165).
- Public documentation on `PersistentAttachOutcome`, `ensure_exists_in`,
  `list_in`, `upsert_stub_in`, `set_metadata_in` and `reconcile_in` no longer
  links to private items, which made `cargo doc` fail under
  `RUSTDOCFLAGS="-D warnings"`. The prose still names the internal helpers; it
  just no longer tries to hyperlink to items a reader cannot navigate to.
- **Hyper-format export side-effect correction (supersedes the older
  Unreleased note below).** Export does not mutate its source database, but it
  creates or replaces the requested destination `.hyper` file and materializes
  every user table from the selected source. It remains available under
  `--read-only`; this is destination creation, not a raw database-file copy.
- **Daemon health-port targeting and diagnostics.** Explicit
  `daemon status --port` now probes that exact health port, discovered-daemon
  error reports target the effective health port, and best-effort health I/O
  no longer retains the engine mutex.
- **Health-listener connections could be torn down before the client sent its
  first byte, on macOS and other BSD-derived kernels.** `HealthListener::bind`
  puts the listening socket in non-blocking mode so its accept loop can poll
  for shutdown; on BSD kernels (unlike Linux) `accept()` propagates that
  `O_NONBLOCK` flag to the accepted socket, so the connection handler's first
  `read_line` returned `WouldBlock` in microseconds and closed the connection
  before a client had a chance to write a command. Liveness checks, restart
  reporting, and heartbeats all depend on this connection surviving long
  enough to receive one line, so accepted connections are now explicitly
  forced back into blocking mode.
- **Attachment contention is actionable for persistent *and* user attaches.**
  A lock conflict (SQLSTATE `55006`, or a legacy "already attached" / "file is
  locked" phrase from older hyperd) now returns `RESOURCE_BUSY` with the
  effective path, preserved Hyper diagnostic/SQLSTATE, doctor guidance, and
  non-accusatory possible-owner recovery. Previously only the reserved
  persistent-attach path was reclassified, so a user `attach_database` on a
  file another process already held surfaced as a generic `SqlError`; it now
  routes through the same attach-context mapper. Unrelated `55006` errors
  outside the attach context retain their existing mapping.
- **Chart geometry, ranges, and positive-log rendering.** Bars always treat x
  as categorical, honor y ranges and in-range baselines, and validate finite,
  increasing, representable spans. Horizontal ordering/grouping/labels and
  extreme positive log ranges now render without zero baselines, collapsed
  endpoints, unbounded ticks, or silently misrepresented measures.
- **Chart color parsing no longer panics on non-ASCII input.** A `color_map`
  value that is six *bytes* but not six ASCII characters (e.g. `"1é234"`, where
  `é` is two UTF-8 bytes) passed the length check and then panicked on a
  non-char-boundary byte slice, aborting the request. `parse_hex_color` now
  rejects any non-ASCII string up front and returns a normal "invalid color"
  result instead.
- **`doctor` no longer presents an illustrative client-log path as fact in
  ephemeral-only mode.** With no persistent database configured, the reported
  client log path is derived from the doctor invocation's own temporary
  directory and process id, so it cannot identify a separate running MCP
  server's log directory. `doctor` now emits a warning making that explicit
  instead of implying the path locates a live session.
- **npm launcher reports the correct manifest in the same-directory
  fallback.** When the binary is resolved next to `bin.js` (rather than in a
  platform subpackage), the launcher metadata's `platform.package_path`
  pointed at a nonexistent platform-subdirectory `package.json`. It now
  resolves the manifest that actually sits beside the binary, so
  `doctor`/launcher diagnostics report a real path.
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
- **A leading `NULL` x no longer downgrades a whole chart's axis, and no
  longer breaks it either.** Two separate defects on the line/scatter x
  axis. First, a NUMERIC value too large for `f64` to round-trip serializes
  as an exact-text JSON *string* (see the NUMERIC entry above); the y axis
  handles that through a typed sidecar, but the x axis read the raw JSON, so
  `detect_line_x_mode` saw a non-numeric, non-temporal string and
  misclassified a quantitative axis as categorical — flattening it to
  evenly-spaced ordinal positions with no warning. Second, detection sampled
  row 0 only, so one blank x on an otherwise numeric or temporal column
  flipped the entire chart's interpretation and plotted the `NULL` row as a
  real point with a blank label. `execute_chart_query_to_json` now builds
  the same typed `ChartMeasureValue` sidecar for line/scatter's x column
  that it already built for the measure column, and `detect_line_x_mode`
  prefers that sidecar, falling back to raw-JSON sampling only when none was
  requested (e.g. the public `render_chart` API).

  Detection now looks past `NULL` x values to classify the column, and
  `group_chart_series` **skips those same rows** when the axis is numeric or
  temporal, so a `NULL` x is dropped from the plot rather than positioned.
  Both halves are required: `chart_measure_coordinate_and_label` maps a
  `NULL` x to `SCHEMA_MISMATCH`, so teaching detection alone to look past a
  blank x would have made the row detection ignores the row grouping fails
  on — turning a chart that previously rendered (as categorical) into a hard
  error. `rows_plotted` counts only the rows actually plotted, and an
  all-`NULL` x column under numeric mode reports `EMPTY_DATA`. Categorical
  axes are unchanged: there a `NULL` remains a legitimate blank-labelled
  category. Part of
  [issue #277](https://github.com/tableau/hyper-api-rust/issues/277).
- **Exporting over a `.hyper` file another process holds open reported a
  permissions failure instead of a busy resource (Windows).** `export`
  deletes an existing target before recreating it, and Windows refuses to
  unlink a file another process holds open — so a contended export target
  failed with `PERMISSION_DENIED "Cannot remove existing target … (os error
  32)"` and the guidance *"Check file permissions on the source or target
  path"*, which points at something the user will find nothing wrong with.
  That delete is now classified: a sharing violation
  (`ERROR_SHARING_VIOLATION` / `ERROR_LOCK_VIOLATION`, or `EBUSY` anywhere)
  surfaces as `RESOURCE_BUSY`, whose guidance names the actual remedy —
  close the holding process or copy the file. A genuine permissions failure
  still reports `PERMISSION_DENIED`. Unix is unaffected: `unlink` succeeds
  regardless of open handles, which is why this only ever appeared on
  Windows.

  The related routing of `export`'s `CREATE DATABASE` / `ATTACH DATABASE`
  through the same attach-context error mapper `attach_database` uses (and
  the same for the `CREATE DATABASE IF NOT EXISTS` before `attach`'s already
  mapped `ATTACH`) is kept, but is a **defensive consistency change rather
  than a fixed user-visible regression**: because the delete above runs
  first, those statements never see a contended target — on Unix the path is
  always freed, and on Windows the delete fails before them. The earlier
  claim that 0.7.2 returned `RESOURCE_BUSY` from those statements for a
  contended export was never substantiated and is withdrawn; the real
  failure was one step earlier, at the delete. Part of
  [issue #277](https://github.com/tableau/hyper-api-rust/issues/277).
- **Corrected the justification on chart.rs's `cast_precision_loss`
  suppression.** The file-wide `#![allow]` claimed values approaching 2^53
  "would saturate to Infinity in the chart anyway" — they don't; `f64`
  represents them finitely and merely stops distinguishing adjacent
  integers, which is exactly why `ChartMeasureValue` (and this PR's x-axis
  sidecar) exists. Replaced with `#[expect(clippy::cast_precision_loss)]` on
  each of the eight functions that actually cast an integer to `f64`, each
  with the true, function-specific reason. Every one of those casts is a
  bounded count, index, or calendar timestamp — categories, series, bin
  counts and indices, epoch seconds — and every one is exact at the
  magnitudes reachable here, which is the property that matters. Two of them
  *are* plotted geometry rather than pure layout: `parse_temporal`'s
  timestamp is the plotted x coordinate, and a histogram bin's count is the
  plotted bar height. Both are lossless (a row tally is bounded by the
  50,000-row chart limit, and epoch seconds are nowhere near 2^53), so the
  suppression is still sound — the previous blanket phrasing "never a
  plotted data value" simply claimed more than was true. Measure values
  proper still flow through `ChartMeasureValue`/`ChartPoint::y_label` as an
  already-typed `f64` with exact display text alongside, never reconstructed
  from an integer cast. `#[expect]` (vs `#[allow]`) also means the
  suppression is now checked for staying necessary. Part of
  [issue #277](https://github.com/tableau/hyper-api-rust/issues/277).
- **`render_chart` no longer trusts its caller to bound `ChartOptions::bins`.**
  `bins` is a bare `pub u32` and `draw_histogram` allocates one counter per
  bin, so a caller passing `u32::MAX` through the public Rust API would have
  attempted a ~34 GB allocation. The MCP `chart` tool clamped its own `bins`
  parameter to `1..=500`, but the renderer did not, applying only `.max(1)`.
  The cap now lives in `chart.rs` as `MAX_HISTOGRAM_BINS` and is enforced by
  the renderer, with the MCP tool clamping to the same constant.
- **A panicking tool call no longer bricks the server for the rest of the
  process's lifetime.** `with_engine` holds a `std::sync::MutexGuard` across
  the tool closure it invokes; a panic propagating out of that closure dropped
  the guard mid-unwind and poisoned the engine mutex, and every subsequent
  tool call then failed with `InternalError "Lock poisoned"` until the process
  restarted. The engine lock now recovers from poisoning the same way it
  already recovers from `ConnectionLost`: it discards the (possibly
  mid-mutation) `Engine` behind the poisoned guard, clears the poison flag,
  and rebuilds a fresh engine on the next call — never reusing a value a panic
  may have left in a broken state.

  Recovery runs at every site that locks the engine, not just the one the
  panic came through, because the server hands the same handle to background
  watchers via `engine_handle()`. A watcher's connection-lost pool rebuild
  previously failed with `InternalError "Engine lock poisoned"` and its
  `_table_catalog` bookkeeping was silently skipped until some unrelated tool
  call happened to clear the flag — indefinitely, for an unattended ingest
  where no tool call may arrive. A watcher now sees the recovered (empty)
  engine slot and returns its transient, retryable "Engine not initialized"
  error instead. The engine-construction single-flight lock recovers too, so a
  panic during `Engine::new`, attachment replay, or the internal
  `debug_assert!` cannot re-brick the server through a second mutex.

  `status`, which reads the engine with `try_lock` so it never waits behind a
  slow data-plane call, no longer reports a poisoned mutex as contention. It
  previously answered `engine_busy: true` — whose description tells the client
  to retry later — permanently after a panic, with no call in flight and no
  way for the flag to clear itself. It now recovers and reports the empty
  engine as degraded, which is the same shape of answer but an honest one.

  **Recovery destroys the session's ephemeral tables.** Discarding the
  `Engine` runs its `Drop`, which deletes the temp directory holding the
  ephemeral primary database, so every table loaded without `persist: true` is
  lost and the rebuilt engine starts empty. Attached databases survive —
  they are replayed onto the new engine and `persistent` is re-attached. This
  is a deliberate trade rather than an oversight: an `Engine` a panic caught
  mid-mutation cannot be trusted, and handing it back out silently is worse
  than losing scratch tables. Load with `persist: true` if data must survive a
  panicking tool call. Fixes
  [issue #266](https://github.com/tableau/hyper-api-rust/issues/266).
- **`discover()` rejected a live daemon over an unrecognized `identity`
  block, defeating forward compatibility.** The client fast path deserialized
  the whole `DaemonRecord` (added in 0.7.3) — including the nested `identity`
  object — and discarded the enrichment immediately after
  (`record.info().clone()`), so the added strictness bought nothing while
  costing compatibility across daemon/client version skew: a retyped
  `identity`, a reshaped `executable_path`, or an `identity` missing
  `executable_path` each made a well-formed, live daemon's `daemon.json` fail
  to parse, and `discover()` returned `None` for a healthy process.
  `wait_for_daemon` (`spawn.rs`) then timed out and silently fell back to a
  private `hyperd`, `status_degraded` (`server.rs`) reported no daemon, and
  `Engine::is_running` returned false. `discover()` now reads the file once
  and parses those bytes twice: first as the strict `DaemonRecord`, which
  remains the primary path, and — only when that fails — as a bare
  `DaemonInfo`, which ignores unknown fields and never trips over a nested
  object this fast path does not need. A record that only the tolerant
  fallback could read is deliberately left on disk when its health check
  fails, instead of being stale-cleaned: such a record is exactly what a
  *newer* daemon looks like to an older client, the daemon rewrites
  `daemon.json` only on `hyperd` restart, and deleting it over one failed
  300 ms PING would hide a live daemon from every client on the machine, not
  just this one. A strictly parsed record on a dead port is still cleaned up
  as before. The doctor's separate raw reader is unaffected and keeps
  the strict contract, since it deliberately surfaces a malformed `identity`
  block as a diagnostic. Fixes
  [issue #270](https://github.com/tableau/hyper-api-rust/issues/270).
- **The "atomic" discovery-file write wasn't atomic, on the strength of a
  false premise about Windows.** `write_discovery_record` deleted the
  existing `daemon.json` before renaming the temp file into place, with a
  comment claiming "`rename` fails if target exists" on Windows — but
  `std::fs::rename` replaces an existing target on both Windows
  (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`, falling back to
  `SetFileInformationByHandle`) and Unix. The unnecessary delete opened a
  window where the file did not exist at all; since `try_restart_hyperd`
  rewrites this file on every `hyperd` restart, a concurrent `discover()`
  could observe `Missing` mid-restart. The write is now a single `rename`,
  matching what the function's doc comment already promised.
- **An oversized-but-well-formed discovery record was reported as
  `Malformed`.** The doctor's bounded raw reader rejected any file over 64
  KiB before attempting to parse it, so a file that was perfectly valid JSON
  — just unexpectedly large — sent the user to fix "invalid JSON" that
  parsed fine. `RawDiscoveryRead` gained an `Oversized` variant so the doctor
  reports what actually happened instead.
- **A daemon peer that closed a health-command connection without writing
  anything was reported as a successful, empty response.** `daemon_stop`
  printed `Daemon responded:` with nothing after it and exited 0, which a
  calling script would read as a successful stop. `send_command_with_timeout`
  now returns `UnexpectedEof` when the connection ends before any bytes are
  read. `doctor`'s separate `send_doctor_command` read loop had the same
  shape and now classifies a silent close identically. This changes one
  `doctor` warning: a recorded daemon candidate that accepts the connection
  and then closes without answering is now reported as
  `daemon_discovery_candidate_unreachable` ("did not return fresh enriched
  STATUS") rather than `daemon_status_malformed` ("returned malformed or
  unenriched STATUS"), which is what actually happened — the peer sent no
  response to be malformed. Fixes 3 of the 5 items in
  [issue #275](https://github.com/tableau/hyper-api-rust/issues/275).
- **After restarting `hyperd`, the daemon advertised the new endpoint over the
  health port before writing it to the discovery file.** `try_restart_hyperd`
  copied the endpoint into the in-memory `DaemonInfo` that `STATUS` serves and
  released that lock *before* rewriting `daemon.json`. Since
  `discovery::discover()` — the path every client's `Engine::new` takes —
  reads the file, a client discovering inside that window connected to the
  `hyperd` the daemon had just dropped. The write failing was worse than the
  window: the error return happened with the new endpoint already published to
  `STATUS` and the replacement `HyperProcess` dropped on the way out, leaving
  `STATUS` durably naming a `hyperd` the daemon itself had killed. The
  discovery file is now persisted first and the `DaemonInfo` flipped second,
  both under one lock, so observing the new endpoint through either channel
  implies a live `hyperd` behind it and no `STATUS` reader can see an endpoint
  the file has not committed. Fixes
  [issue #284](https://github.com/tableau/hyper-api-rust/issues/284).

### Security

- **Daemon state files are now restricted to the owning user.** The state
  directory (`~/.hyperdb`, or `HYPERDB_STATE_DIR`) is created `0700` and
  `daemon.json` `0600` on Unix, where previously both took their mode from the
  process umask — commonly `0755` and `0644`. `daemon.json` names the `hyperd`
  endpoint, so it is owner-only from the moment it exists: the mode is set on
  the atomic write's temp file *before* any content is written, and the
  subsequent `rename` replaces the target's inode, which also tightens a record
  an earlier release left readable. `logs/` gets the same `0700` treatment,
  since `hyperd` writes its own diagnostic logs there under its own umask and
  those records name the endpoint too — restricting the directory covers a
  file this process does not own and has not seen yet. Directories and the
  regular files directly inside them are both corrected when an earlier run
  left them loose, rather than accepted as-is: closing the directory does
  nothing about a log file already in it, and `logs/` is where the daemon's own
  log and `hyperd`'s rotated logs sit. The sweep is one level deep, skips
  anything that is not a regular file, and warns rather than failing, so it
  cannot follow a link out of the directory or walk into whatever
  `HYPERDB_STATE_DIR` names.
- **The discovery file's temp file is created exclusively, so the mode always
  applies to a file of our own.** `write_discovery_record` wrote
  `daemon.json.tmp` with a plain create, which opens whatever already bears
  that name and leaves the intended mode dependent on what that turns out to
  be — a leftover temp file kept its own mode, and a name that resolved
  somewhere else was written through. It is now unlinked and recreated with
  `O_CREAT | O_EXCL` and `O_NOFOLLOW`, so the record always lands on a regular
  file inside the state directory, created with the mode it is meant to have;
  if the name cannot be created cleanly the write fails rather than publishing
  anyway.
- **A filesystem that cannot represent Unix modes no longer costs the daemon
  its discovery file.** `chmod` is refused outright on a mount whose
  permissions come from `fmask`/`dmask` rather than from each file — SMB or
  NFS, exFAT or vfat, some container bind-mounts. Tightening the *directory*
  already warned and carried on there, but setting the mode on `daemon.json`
  propagated the refusal, so on exactly those filesystems the daemon warned
  about the directory and then could not publish a record at all — turning a
  working-if-loose setup into a startup failure. A refusal is now reconciled
  against the mode actually on disk: if the file already reads back with
  nothing granted to group or other, the record is protected and is published;
  if it reads back wider, the error stands and nothing is published. Windows
  relies on the ACL that `%USERPROFILE%` subdirectories inherit, which already
  excludes other interactive users.

## [0.5.0] - 2026-06-07

### Added

- The `status` tool now reports an `engine` block with the backing `hyperd`
  connection: `mode` (`daemon` or `local`), `hyperd_endpoint` (the libpq
  endpoint queries run against), and `daemon_health_port` (the shared daemon's
  control/lock port, `null` in local mode).
- **Single-instance `hyperd` daemon** — by default, all MCP clients now
  share one `hyperd` process per user instead of each spawning their own.
  Multiple AI clients (Claude Code, Cursor, VS Code Copilot, etc.) can
  access the same persistent databases simultaneously with reduced
  resource overhead. The daemon auto-spawns on first client connect and
  stays resident (idle shutdown is opt-in — see below). Pass `--no-daemon`
  to opt out.
- **Identity-checked daemon discovery.** Clients verify a daemon by sending
  `PING` and requiring a `PONG hyperdb-mcp <version>` reply (matched on exact
  tokens, not a string prefix) before trusting it — a TCP connection alone is
  no longer sufficient, so an unrelated process occupying the port is no
  longer mistaken for the daemon.
- **Port scanning.** The daemon health/lock port now defaults to scanning
  upward from **7485** (16 ports), using the first free one; the old fixed
  default 7484 collided with `hyperd`'s conventional gRPC port. Set
  `HYPERDB_DAEMON_PORT` to pin an exact port (disables scanning). `daemon
  status` / `daemon stop` locate the daemon via discovery + scan, so they
  work regardless of which port it landed on.
- **Newer-client version takeover.** A starting client built from a strictly
  newer `hyperdb-mcp` version stops and replaces an older running daemon
  (and its `hyperd`), so upgrades take effect immediately instead of waiting
  for the old daemon to exit. Equal or older versions reuse the daemon.
- **Daemon stays resident by default.** Idle shutdown is now opt-in via
  `--idle-timeout <SECS>` or `HYPERDB_DAEMON_IDLE_TIMEOUT`; with neither set
  the daemon (and `hyperd`) stay warm, eliminating the connection error and
  "hyper restarting, please retry" round-trip a client previously hit after
  a 30-minute idle shutdown.
- New `daemon` subcommand: `hyperdb-mcp daemon status` / `daemon stop`.
- New environment variables: `HYPERDB_STATE_DIR`, `HYPERDB_DAEMON_PORT`,
  `HYPERDB_DAEMON_IDLE_TIMEOUT`.
- Ephemeral databases now `DETACH DATABASE` before deletion on session
  end — required on Windows where the OS enforces file locks on open
  Hyper files.
- **Daemon-side `hyperd` restart on crash.** The daemon polls `hyperd`
  every 5 seconds via `Child::try_wait()` and automatically restarts it
  if the process has exited, atomically updating the discovery file
  with the new endpoint. Clients reconnect transparently via the
  existing `ConnectionLost` recovery path. New `REPORT_HYPERD_ERROR`
  health-protocol command lets clients fast-path the signal when they
  detect a dead hyperd before the daemon's polling tick. Restart
  attempts are rate-limited to 3 per 60 seconds; exceeding the limit
  triggers daemon shutdown so the user sees the failure clearly
  rather than spinning silently.
- **Two-database engine model.** Every session now has both an
  ephemeral primary (created fresh per-session, deleted on exit) AND
  a persistent attachment under the alias `"persistent"`. Unqualified
  SQL routes to the ephemeral primary (the LLM's scratch space);
  fully-qualified SQL like
  `INSERT INTO "persistent"."public"."customers" ...` writes to
  storage that survives across sessions.
- **Platform-default persistent path.** When `--persistent-db` is
  unset, the persistent file lives at the platform data directory:
  `~/Library/Application Support/hyperdb/workspace.hyper` on macOS,
  `~/.local/share/hyperdb/workspace.hyper` on Linux,
  `%APPDATA%\hyperdb\workspace.hyper` on Windows. Override with
  `HYPERDB_PERSISTENT_DB`.
- **New CLI flags:** `--persistent-db <PATH>` (replaces `--workspace`,
  which is kept as a deprecated alias with a stderr warning); and
  `--ephemeral-only` to skip the persistent attachment entirely.
- The `_table_catalog` and `_hyperdb_saved_queries` meta-tables now
  live in the persistent attachment instead of the connection's
  primary, so saved queries automatically persist across sessions
  without any flag toggling.
- **Per-tool `database` parameter.** `query`, `execute`, `load_data`,
  `load_file`, `describe`, `sample`, `chart`, and `export` now accept
  an optional `database: string` parameter. Omit (or pass `"local"`)
  to target the ephemeral primary; pass `"persistent"` to target the
  durable database (case-insensitive), or any user-attached writable
  alias. Tools that build their own SQL fully qualify table
  references; tools that take user-provided SQL temporarily redirect
  `schema_search_path` for the call duration via an RAII guard that
  always restores on drop. (`query_data` and `query_file` materialize
  inline data into a temp table; they don't accept the parameter.)
- **`persist: true` shorthand on ingest tools.** `load_data` and
  `load_file` accept a boolean `persist` flag — equivalent to
  `database: "persistent"` — for ergonomic LLM workflows like
  `load_data({ table: "x", data: "[...]", persist: true })`. If
  both `database` and `persist` are set, `database` wins. Combining
  `persist: true` with `--ephemeral-only` returns a clear
  `InvalidArgument` error.
- **Cross-database merge.** `load_file` with `mode: "merge"` now
  accepts any writable `database`. The merge path keeps the temp
  table inside the target database so DELETE-USING and INSERT-SELECT
  stay single-DB — no cross-database DML is required. Engine
  helpers `table_exists_in`, `column_metadata_in`, and
  `alter_table_add_columns_in` carry the routing.
- **`export(format="hyper", database=...)`.** The hyper-format
  export now snapshots whichever database the caller named (via a
  new `ExportOptions.source_db` field plumbed into
  `populate_export_target`). Default behavior (snapshot primary)
  unchanged.
- **`load_files` and `watch_directory` accept `database` / `persist`.**
  Their connection pool now opens the resolved target's `.hyper`
  file directly as its workspace, so unqualified ingest SQL routes
  into the right database without further plumbing. The watcher's
  reconnect-recovery path re-resolves the target so a hyperd
  restart picks the right file. `WatcherHandle` records its
  `target_db`; `detach_database` rejects with `InvalidArgument` if
  any active watcher targets the alias (call `unwatch_directory`
  first to release it).
- **Per-database `_table_catalog`.** Every writable database
  receives its own catalog, lazily seeded on first ingest. The
  catalog CRUD API gains `*_in(target_db)` siblings
  (`ensure_exists_in`, `upsert_stub_in`, `set_metadata_in`,
  `get_in`, `list_in`, `delete_for_in`, `reconcile_in`). The
  per-engine catalog-presence cache is now keyed by canonical
  alias (`Mutex<HashMap<String, bool>>`); detach clears the entry.
- **`set_table_metadata.database` parameter.** Routes the catalog
  write to the named database's `_table_catalog`. Read-only
  attachments are rejected up front with a clear "re-attach with
  writable:true" message.

### Changed (breaking — pre-1.0)

- `Engine::catalog_present_in_persistent` → `Engine::catalog_present_in(alias, prober)`.
- `Engine::mark_catalog_present` → `Engine::mark_catalog_present_for(alias)`.
- `Engine::catalog_present_cache` field shape: `Mutex<Option<bool>>` → `Mutex<HashMap<String, bool>>` keyed by lowercased alias.
- New `Engine::clear_catalog_cache_for(alias)` paired with `detach_database`.
- `table_catalog::ensure_exists_in_database(engine, alias)` is now a deprecated wrapper over `ensure_exists_in(engine, Some(alias))`.
- **Attach aliases are canonicalized to lowercase at attach time.**
  `attach_database(alias="MyDB", …)` now stores `"mydb"` in the registry,
  and `Engine::resolve_target_db` returns the lowercase form for any
  alias. Eliminates the latent footgun where attaching as `"User_DB"`
  and detaching as `"user_db"` silently no-op'd while leaving the
  catalog-presence cache populated. Affects users who relied on
  case-sensitive registry distinctness — pre-1.0, no migration is
  shipped.

### Removed

- **`--bare` flag.** Catalog seeding is now uniform: created when MCP
  creates a fresh `.hyper`, never touched on existing files. Users
  who want a pristine `.hyper` for export can `DROP TABLE _table_catalog`
  once after creation; subsequent opens won't recreate it.

### Performance

- **Per-database `_table_catalog` presence cache.** Catalog reads/writes
  used to round-trip a `pg_catalog.pg_tables` probe on every call;
  now the existence check is cached per (engine, alias) and primed
  immediately after `CREATE TABLE IF NOT EXISTS`. `detach_database`
  clears the alias's entry so a re-attach to a different file isn't
  served stale.
- **Cross-process catalog write safety via optimistic concurrency.**
  `upsert_stub_in` now uses UPDATE-then-conditional-INSERT instead
  of DELETE+INSERT in a transaction. Each statement is individually
  atomic at the `hyperd` level, so multiple MCP server processes
  sharing the same persistent database via the daemon can no longer
  produce duplicate `_table_catalog` rows from concurrent ingests.
  The UPDATE path also eliminates one round-trip (the pre-read of the
  existing row to preserve prose fields — UPDATE preserves them
  implicitly by only touching mechanical columns).

### Fixed

- **Chart x-axis tick label thinning.** Long categorical line/scatter
  charts (e.g. a 90-point hourly TIMESTAMP series) used to render with
  only ONE visible x-axis label. The old logic blanked individual
  labels at non-step indices, but `plotters` picks its own tick
  *positions* on the float axis and rounds them to integer indices —
  so almost none of the chosen ticks landed on a kept index, and the
  formatter returned empty strings for the rest. The chart layer now
  computes a target tick count from chart width and label sizes and
  passes that count to `plotters` via `.x_labels(N)`; every drawn
  tick carries its real label. Same `+00:00` suffix stripping for
  shared TIMESTAMPTZ offsets is preserved (now isolated in
  `strip_shared_tz_suffix`).
- **Line / scatter charts over `DATE`, `TIMESTAMP`, and `TIMESTAMPTZ`
  columns now use a proportional time axis** instead of the previous
  categorical-with-evenly-spaced-ticks behavior. Real-world time gaps
  between data points are now reflected in the chart's x-axis: a
  series at `2026-05-01 08:00`, `2026-05-01 12:30`, `2026-05-02 06:15`
  shows the 4.5-hour and 17.75-hour gaps proportionally instead of
  flattening every interval to the same width. Tick labels are
  formatted via `chrono` in a form that matches the input kind:
  `%Y-%m-%d` for DATE, `%Y-%m-%d %H:%M:%S` for TIMESTAMP, and
  `%Y-%m-%d %H:%M:%S%:z` for TIMESTAMPTZ (the offset captured from the
  first row, so a uniformly-`+05:30` series reports IST throughout).
  Set `x_as_category: true` to opt out and force the previous
  categorical layout (e.g. for charts where evenly-spaced bins are
  more readable than proportional gaps). TEXT x columns continue to
  render categorically as before. Bar charts are unaffected — they
  remain categorical regardless of x type, which matches reader
  expectations for grouped data.
- **Watcher recovery after hyperd restart.** The watcher's connection
  pool now auto-rebuilds when a per-file ingest hits a connection-lost
  error (typically after the daemon restarts hyperd). Each ingest gets
  one retry on a fresh pool; persistent failures still flow into the
  standard `failed/` move so a single broken file can't pin the
  watcher in retry loops.
- **`execute` now reconciles the user-attached target's `_table_catalog`.**
  Pre-fix, raw DDL like `execute(database="foo", sql="DROP TABLE bar")`
  removed the table from the user-attached DB but left its stub row
  stranded in `"foo"."public"."_table_catalog"` indefinitely (bootstrap
  reconcile and the post-execute reconcile both walked persistent
  only). `after_execute_catalog_update` now reconciles persistent
  first and then the user-attached target if non-persistent. Gated on
  `is_structural_sql` (CREATE / DROP / ALTER / TRUNCATE / RENAME) so
  per-row `INSERT` / `UPDATE` / `DELETE` no longer triggers a
  workspace-wide catalog scan on every call.
- **`copy_query.target_database` now canonicalizes mixed-case aliases.**
  Pre-fix, `attach_database(alias="My_DB")` (which the registry stores
  as `"my_db"` after the alias-canonicalization change) followed by
  `copy_query(target_database="My_DB")` rendered SQL referring to
  `"My_DB"` and Hyper rejected it with "database does not exist"
  (Hyper is case-sensitive on quoted identifiers). The tool now
  lowercases `target_database` after the `LOCAL_ALIAS` filter so the
  registry lookup AND the qualified-SQL build path agree on the
  canonical lowercase form.
- Query results now preserve the sign of negative `NUMERIC`/`DECIMAL` values
  with magnitude less than 1. Previously a value like `CAST(-0.5 AS
  numeric(10,4))` was serialized to JSON as `0.5` because `row_value_to_json`
  stringifies NUMERIC via `Numeric::to_string()`, whose `Display` impl dropped
  the sign for sub-unit magnitudes (fixed in `hyperdb-api-core`). This silently
  flipped the sign of correlations, 0–1 indices, and regression residuals.

## [0.1.1] - 2026-05-13

### Added

MCP tools — query and execute:

- `query` — read-only SELECT / WITH / EXPLAIN / SHOW / VALUES
- `execute` — DDL / DML (CREATE, INSERT, UPDATE, DELETE, DROP, etc.)
- `query_data` — ingest inline JSON or CSV and run one SQL query
- `query_file` — same as `query_data` but reads from a file path

MCP tools — load:

- `load_file` — load one CSV / JSON / JSONL / Parquet / Arrow IPC file into a workspace table
- `load_files` — load many files in parallel
- `load_data` — load inline JSON or CSV into a named workspace table
- `load_iceberg` — load an Apache Iceberg table by absolute root-directory path

MCP tools — inspect:

- `describe` — list workspace tables or describe one table
- `sample` — return schema + first N rows of a table
- `inspect_file` — dry-run schema inference on a file before loading
- `status` — plugin health, workspace path, table count, total rows, disk usage, watchers, attached databases, read-only flag

MCP tools — export and visualize:

- `export` — write a table or query result to a file (Parquet, Iceberg, Arrow IPC, CSV, `.hyper`)
- `chart` — render PNG/SVG charts (bar, line, scatter, histogram) from a SQL query via `plotters`
- `copy_query` — run a SELECT across local + attached databases and insert the result into a target workspace table (`create` / `append` / `replace` modes)

MCP tools — saved queries:

- `save_query` — save a named read-only SQL query for later reuse, exposed as MCP resources
- `delete_query` — delete a previously saved query

MCP tools — table metadata:

- `set_table_metadata` — update prose metadata (source URL, purpose, etc.) on a workspace table

MCP tools — multi-database:

- `attach_database` — attach an additional `.hyper` database under an alias for cross-database JOINs
- `detach_database` — detach a previously attached database
- `list_attached_databases` — list current attachments

MCP tools — directory watch:

- `watch_directory` — auto-ingest matching files via `.ready` sentinel files
- `unwatch_directory` — stop watching a previously registered directory

MCP tools — introspection:

- `get_readme` — return the LLM-facing README for orientation at the start of a session

MCP prompts:

- `analyze-table` — guided table analysis prompt
- `compare-tables` — side-by-side table comparison prompt
- `data-quality` — data-quality assessment prompt
- `suggest-queries` — query suggestion prompt

Library modules:

- `attach` — registry of attached `.hyper` databases for cross-database JOINs
- `chart` — chart rendering via `plotters`
- `engine` — `HyperProcess` lifecycle, connection management, table CRUD, query execution
- `error` — structured error codes with LLM-friendly recovery suggestions
- `export` — write query results to CSV, Parquet, Arrow IPC, or `.hyper` files
- `ingest` — inline JSON and CSV loading via `INSERT` and `COPY`
- `ingest_arrow` — Parquet and Arrow IPC file loading
- `inspect` — dry-run file inspection powering the `inspect_file` tool
- `lakehouse` — Apache Iceberg ingest via `hyperd`'s native external-format reader
- `readme` — static LLM-facing README returned by `get_readme`
- `saved_queries` — named read-only SQL with persistence
- `schema` — three-tier schema inference (exact / structural / heuristic) with user overrides
- `server` — MCP tool definitions and `rmcp` server handler
- `stats` — performance telemetry (throughput, timing) on responses
- `subscriptions` — MCP resource-update notifications
- `table_catalog` — user-visible table metadata (`_table_catalog` tracking)
- `version` — compile-time version strings with git-hash suffix
- `watcher` — directory monitoring for incremental ingest via `.ready` sentinels

Other:

- `hyperdb-mcp` CLI binary for invoking the MCP server (workspace, ephemeral, read-only modes)
- Zero feature flags — all capabilities always available
