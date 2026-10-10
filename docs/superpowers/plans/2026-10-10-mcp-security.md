# hyperdb-mcp security fixes Implementation Plan

**Goal:** Close the four decision-9 findings in `hyperdb-mcp`: the `--read-only` bypass, the destructive export default, the unauthenticated loopback health protocol, and the takeover race.

**Spec:** [`docs/superpowers/specs/2026-10-10-mcp-security-design.md`](../specs/2026-10-10-mcp-security-design.md). It holds the design, the platform details, and the measured facts (Hyper's SQL behaviour, the file magic, the race timeline). Read it first and do not re-derive any of it.

**Tech stack:** std `UnixListener` / `UnixStream`, `libc` (`poll`, `flock`; already a Unix dependency), `windows-sys` 0.61 (new direct Windows dependency; 0.61.2 is already in `Cargo.lock`), `tokio::sync::Notify` (add tokio's `sync` feature to `hyperdb-mcp`).

## Global constraints

- **Engine.** Pinned hyperd only, `HYPERD_PATH="$PWD/.hyperd/current"` absolute. No invented hyperd flags.
- **Code rules.** No narrowing integer `as` casts (AGENTS.md reminder 7); `libc` pid/fd conversions go through `TryFrom`. Library code returns errors. Lifetimes get descriptive names. Every `unsafe` block has a `// SAFETY:` comment.
- **Commits.** Explicit `git add <files>`; never stage `.agents/` or `.codex/`. Conventional Commits, `!` on breaking iterations. No push.
- **MSRV 1.88.** `cargo +1.88 check --workspace --all-targets` stays clean (so no `File::try_lock`).
- **Windows.** No local Windows C toolchain: `cargo check -p hyperdb-mcp --target x86_64-pc-windows-msvc` fails in `ring`'s build script (measured: `cc` exit 1, cargo exit 101). So the modules with Windows code, `daemon/control.rs`, `daemon/lock.rs` and `src/file_identity.rs`, depend only on `std`, `libc` and `windows-sys` (no `crate::` imports), and each iteration that touches them compiles them for Windows in a scratch crate (`/tmp/mcpsec/wincheck`, `#[path = "..."] mod control;` and so on, `windows-sys` with the same features) with `cargo clippy --target x86_64-pc-windows-msvc -- -D warnings`. The `x86_64-pc-windows-msvc` target is installed (checked). Windows-only code elsewhere stays minimal and `cfg`-gated. CI's Windows job is the real check, and the user pushes.
- **Gate for every iteration:**
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace`
  - `cargo +1.88 check --workspace --all-targets`
  - `cargo test -p hyperdb-mcp` (whole crate; real output, exit code checked via `>log 2>&1; echo $?`)
  - `npx markdownlint-cli2` on touched Markdown, judged against `git show HEAD:<path>`
- **Doc contracts.** Each iteration updates the docs its change makes wrong *and* the doc-contract tests that pin them, in the same commit; never weaken a test to pass. `hyperdb-mcp/src/readme.rs` must stay under 24 000 bytes (`readme_tests.rs` `readme_is_non_trivial`; headroom is about 240 bytes, so trim when adding). The tool catalog must stay under `CATALOG_BYTE_BUDGET` (57 344, `tool_schema_tests.rs`). The read-only and writable catalogs must stay identical. Known pins: the public README's blocked-tools slice (`**blocked:**` up to `- **resources`) must not contain `export` (`readme_tests.rs` ~611); the get_readme rules keep `export`/`hyper` "remain allowed" wording (~617-635); get_readme chart guidance keeps the word `overwrite` (~672); the CLI reference and `daemon --help` pins (`readme_tests.rs` ~740-747, `doctor_tests.rs` ~2268-2302).
- **Changelog.** `hyperdb-mcp/CHANGELOG.md` `## [Unreleased]`: each iteration adds its own bullets, merged into the existing `### Changed` / `### Fixed` / `### Removed` headings (MD024). Do not rewrite the historic entries that carry "health port" (~215, ~473, ~725).

## Iteration 1: read-only SQL

**Files:** `hyperdb-mcp/src/engine.rs` (tokenizer, `classify_statement`, `strip_leading_sql_comments`, doc comments; a `check_embeddable_query` helper), `hyperdb-mcp/src/server.rs`, `hyperdb-mcp/src/export.rs`, `hyperdb-mcp/tests/read_only_tests.rs`, `hyperdb-mcp/tests/end_to_end_mcp_tests.rs`, `hyperdb-mcp/tests/export_tests.rs`, `hyperdb-mcp/src/readme.rs`, `hyperdb-mcp/README.md`, `hyperdb-mcp/src/error.rs` (only if the `READ_ONLY_VIOLATION` suggestion implies those tools accept writes), `hyperdb-mcp/CHANGELOG.md`.

1. Tokenizer and classifier per spec section 1, design point 1. `strip_leading_sql_comments` stops nesting. Unit tests (in `read_only_tests.rs`): every row of the spec's measured table that the classifier decides; `WITH ... DELETE`, `WITH ... INSERT`, `EXPLAIN (ANALYZE) DELETE`, `EXPLAIN (ANALYZE) WITH ... DELETE` not read-only; `EXPLAIN SELECT`, `EXPLAIN DELETE` (no analyze), `WITH x AS (...) (SELECT ...)`, `TABLE t`, dollar-quoted and `E'...'` literals holding `)`, `;` or `DELETE` read-only; a non-nesting comment that hides a statement from a nesting stripper is not read-only; an unterminated literal or comment, an unbalanced `)`, and a token after a depth-0 `;` are `Other`; the `execute` batch validator's view of `EXPLAIN (ANALYZE) DELETE` (DML).
2. `query_data` and `query_file`: before any ingest, reject `!is_read_only_sql(&params.sql)` with `ErrorCode::SqlError` and the `query` wording, naming the tool. `export` in SQL mode: same check, then `check_embeddable_query` (spec point 3: `prepare` alone, dropped), then the two `COPY` sites splice `COPY (\n{sql}\n) TO ...`. Classify the original SQL, not the `replace_identifier` output.
3. End-to-end tests (`TestHarness::start(read_only, ...)`), red-before-green for each new guard:
   - `query_data`, `query_file`, `export`: a `DROP TABLE persistent.public.t` and a `DELETE` are rejected and the table survives, with and without `--read-only`; a plain `SELECT` still works under `--read-only` for each;
   - `query`: `WITH ... DELETE` and `EXPLAIN (ANALYZE) DELETE` are rejected and the rows survive (red on today's classifier);
   - `export` (csv and iceberg): SQL that closes the `COPY` parenthesis and names its own target is rejected; the target named inside the SQL does not exist afterwards, and a persistent DB named there is unchanged;
   - `export`: a query ending in a `--` comment still exports.
4. Docs: README "Read-Only Mode" (the SQL-level rule now names `query`, `query_data`, `query_file`, `export` and `chart`, written **outside** the blocked-tools slice), readme.rs `query_data` / `query_file` / export lines (keep within budget). CHANGELOG `### Fixed`: the four tools' read-only check, the `WITH` / `EXPLAIN (ANALYZE)` / comment classifier holes, the export wrapper escape.

Commit: `fix(mcp): read-only SQL checks follow Hyper's grammar and cover every SQL tool`.

## Iteration 2: export and chart never destroy what they did not write

**Files:** `hyperdb-mcp/src/server.rs` (`ExportParams`, `ChartParams` docs and defaults, export and chart handlers, protected-set builder), `hyperdb-mcp/src/export.rs`, `hyperdb-mcp/src/chart.rs`, `hyperdb-mcp/src/attach.rs` (stale `validate_output_path` doc), new `hyperdb-mcp/src/file_identity.rs`, `hyperdb-mcp/src/lib.rs`, `hyperdb-mcp/Cargo.toml` (`windows-sys` with `Win32_Foundation`, `Win32_Storage_FileSystem` for now), `Cargo.lock`, `hyperdb-mcp/tests/export_tests.rs`, `hyperdb-mcp/tests/end_to_end_mcp_tests.rs` (and any chart test that wrote onto an existing path relying on the old `true` default), readme.rs, README.md, CHANGELOG.md.

1. First, measure the top-level layout of a real Iceberg export (one-off, from an existing test's output) and pin it as the allowed set.
2. Default `overwrite` to `false` for `export` (server.rs ~3311) and `chart`; rewrite both param docs ("Defaults to false: an existing destination is refused with PERMISSION_DENIED; pass true to replace it").
3. Use `validate_output_path`'s returned path for the checks and the write (spec section 2, point 2).
4. `file_identity.rs`: `same_file(a, b) -> io::Result<bool>` (Unix `MetadataExt::{dev, ino}`; Windows `GetFileInformationByHandle` volume serial + file index). The server builds the protected set (persistent, ephemeral, each `AttachSource::LocalFile`) and passes it to `export_to_file` (`ExportOptions::protected_paths`) and to the chart write path. An existing destination that is the same file as a protected one is `INVALID_ARGUMENT`, for every format and for chart, before any other check.
5. Iceberg: replace the `remove_dir_all` block with the shape check (empty, or top-level entries within the measured set with a `metadata/*.metadata.json`) and `ReplaceGuard` (rename to `<name>.hyperdb-mcp-old-<pid>-<n>`; `commit()` deletes the backup; `Drop` without commit removes what is now at `dest`, renames the backup back, and reports the backup path if that fails).
6. Hyper: refuse unless `symlink_metadata` is a regular file starting with `b"Hyper"`; create the database at `<name>.hyperdb-mcp-tmp-<pid>-<n>`, fill and detach it, rename it over `path`; on failure remove the temp file. Keep the `is_file_in_use` → `RESOURCE_BUSY` mapping on the rename.
7. Tests, red-before-green for the non-Iceberg refusal and the protected-file refusal:
   - iceberg over a non-Iceberg directory (with a sentinel file), and over a directory holding a `.hyper` file, is refused with `overwrite=true`, and the contents survive;
   - iceberg over an existing Iceberg export still replaces it (`iceberg_export_overwrite_replaces_directory` keeps passing);
   - iceberg and hyper exports that fail after creating output (a bad `source_db`, which fails after create and attach, export.rs ~538; a SQL error for iceberg) leave the previous destination byte-identical and no temp or backup behind;
   - hyper over a non-Hyper regular file is refused and the file is untouched; over a real `.hyper` it replaces;
   - export (csv and hyper) and chart to the persistent DB, to an attached file, and to a symlink pointing at the persistent DB are refused even with `overwrite=true`;
   - MCP level: `export` and `chart` without `overwrite` onto an existing file are `PERMISSION_DENIED`.
8. Docs: README export parameter table gains `overwrite` (and the missing `format_options`), the chart row default `false`, the `PERMISSION_DENIED` row, the leftover-backup note; readme.rs chart/export lines (keep `overwrite` in the chart guidance). CHANGELOG `### Changed` (both default flips, BREAKING) and `### Fixed` (destructive replace, protected files). Mind the `markdown_bullet(changelog, "hyper-format export")` contract if a new bullet uses that phrase.
9. Windows scratch-crate clippy for `file_identity.rs`.

Commit: `fix(mcp)!: export and chart refuse to overwrite by default and never replace what they did not write`.

## Iteration 3: control transport and lock modules (not wired yet)

**Files:** new `hyperdb-mcp/src/daemon/control.rs`, new `hyperdb-mcp/src/daemon/lock.rs`, `hyperdb-mcp/src/daemon/mod.rs`, `hyperdb-mcp/Cargo.toml` (`windows-sys` features grow to `Win32_Foundation`, `Win32_Security`, `Win32_Security_Authorization`, `Win32_System_Pipes`, `Win32_Storage_FileSystem`, `Win32_System_Threading`, `Win32_System_IO`, `Win32_System_Memory`; tokio `sync`), `Cargo.lock`.

1. `control.rs` per spec section 3, Transport: `HealthEndpoint` (`for_new_daemon`, `from_record`, `as_str`, `Display`), `ControlListener` (`bind`, a bounded-wait accept shaped to slot into `HealthListener::run`'s loop, `wake`, `remove`), `ControlStream` (`Read + Write`, per-operation timeouts), `connect`.
2. `lock.rs`: `DaemonLock::try_acquire(state_dir)` per spec; a module doc saying the file is never unlinked.
3. Unit tests (cfg as needed): bind/connect/round-trip a line; a second `bind` on a live endpoint fails and does not unlink the live socket; a stale socket file is reclaimed; a non-socket file at the path is refused; an over-long path error names `HYPERDB_STATE_DIR`; socket mode is `0600`; a silent peer hits the read timeout; lock: a second in-process acquire is `None`, released on drop, the lock file survives release.
4. Windows scratch-crate clippy for `control.rs` and `lock.rs`.

Modules are `pub` (tests use them; `daemon` is `pub` in the lib — confirm), so no dead-code `expect` is needed.

Commit: `feat(mcp): per-user control transport and daemon lock`.

## Iteration 4: switch the daemon to the control transport

**Files:** `daemon/health.rs`, `daemon/discovery.rs`, `daemon/spawn.rs`, `daemon/run.rs`, `daemon/mod.rs`, `daemon/state_perms.rs`, `main.rs`, `engine.rs`, `server.rs`, `diagnostics.rs`; tests `daemon_tests.rs`, `recovery_tests.rs`, `doctor_tests.rs`, `health_idle_cost_tests.rs`, `engine_tests.rs`, `readme_tests.rs` and the in-file unit tests of the touched modules; docs `hyperdb-mcp/README.md` (daemon section ~321-325, CLI reference ~1044-1060), `hyperdb-mcp/DEVELOPMENT.md` (~244-253 and the discovery/identity sections), `src/readme.rs` if it mentions the port, `.claude/workflows/plan-to-release.js` (~548), CHANGELOG.md.

Roughly 120 references change (daemon_tests ~48, diagnostics ~30, spawn ~15, engine ~11, server ~10); grep `health_port`, `HYPERDB_DAEMON_PORT`, `isolated_daemon_port`, `--port`, `LiveFromScan`.

1. `state_perms.rs`: the ownership and no-group/other-write check (spec section 3, State directory), used by the daemon and the client.
2. `health.rs`: `HealthListener` wraps `ControlListener`; `DaemonState` stores the endpoint at construction plus a SeqCst `AtomicBool` registered flag (spec); `send_command`, `send_command_with_timeout`, `ping_identified`, `report_hyperd_error_to_daemon` take `&HealthEndpoint`. Keep the absolute I/O deadline and the 64 KiB response cap. Rewrite the module doc's security claims to what is now true.
3. `discovery.rs`: `DaemonInfo.health_endpoint: String`; delete the scan machinery listed in the spec; `discover()` = record + identified `PING`, nothing else.
4. `run.rs`: create and check the state dir, then acquire `DaemonLock` with the `STARTUP_WAIT` retry and the debounced "already running" check (spec section 4, point 3; the pre-flight joins the same budget), bind the control endpoint, start hyperd, write the record. Hold the lock until after `drop(hyper_state)`. `DaemonConfig` loses `port`.
5. `spawn.rs`: `ensure_daemon()` per spec (no scan; held lock → wait, free lock → stale cleanup then spawn); `spawn_detached()` without `--port`.
6. `main.rs`: drop `--port` from `daemon`, `daemon stop`, `daemon status`; status prints "Health endpoint:"; rewrite the `daemon --help` text. `engine.rs`/`server.rs`: `daemon_health_endpoint`, status JSON key `daemon_health_endpoint`. `diagnostics.rs`: `health_endpoint`, drop `LiveFromScan` and the scan request/candidate types, `StatusHealthPortMismatch` → endpoint mismatch, report the lock state; still side-effect free.
7. Tests: migrate every TCP fake peer (`CliStatusListener`, `run_controlled_health_peer`, `RunningHealthListener`, `HeldHeartbeatPeer`, health.rs `TestPeer`, …) to `ControlListener` on a per-test endpoint; `TestDaemon` (`daemon_tests.rs` ~2008-2103) drops `HYPERDB_DAEMON_PORT` / `port: 0` and its `Drop` waits for the lock to be free; `DoctorSandbox` drops `HYPERDB_DAEMON_PORT` and gets a state dir short enough for `sun_path` (today 93 bytes + `/daemon.sock` = 105 > 103). Delete the port CLI tests (`daemon_tests.rs` ~600-678, ~699-764, the base-port assertion ~887-889) and `doctor_cli_reports_live_from_scan_via_real_health_listener` (`doctor_tests.rs` ~2060-2114); replace `health_listener_second_bind_same_port_fails` with a lock-based single-instance test. New tests: state dir owned by someone else or group-writable → local mode (Unix; group-writable is testable without root); a client that finds the lock held and no record waits and does not spawn.
8. Docs and doc-contract tests in this same iteration: README daemon section and CLI reference (state dir, socket or pipe, no port, `--port` and `HYPERDB_DAEMON_PORT` gone, `HYPERDB_STATE_DIR` for isolation), DEVELOPMENT.md, the `readme_tests` / `doctor_tests` CLI pins rewritten to the new contract, `plan-to-release.js`. CHANGELOG `### Removed` (`--port`, `HYPERDB_DAEMON_PORT`, port scan) and `### Changed` (health channel transport, `daemon.json` field, status key, doctor fields, the state-dir ownership rule; old clients against a new daemon fall back to local mode).

Commit: `feat(mcp)!: daemon health channel is a per-user Unix socket or named pipe`.

## Iteration 5: retire legacy daemons

**Files:** new `daemon/legacy.rs`, `daemon/discovery.rs` (the legacy-shape hook), `daemon/spawn.rs`, `daemon/mod.rs`, `daemon_tests.rs`, CHANGELOG.md.

1. `legacy.rs` per spec section 3, Legacy daemons: parse the legacy shape; identified `PING`, `STATUS` pid must equal the record pid, else remove the stale record under the lock and send nothing; `STOP`; wait ≤ 15 s for the port to stop answering; on timeout, local mode without a spawn.
2. Tests with an in-test fake legacy peer on `127.0.0.1:0` and a legacy-shaped record: retired and record gone; pid mismatch → no `STOP` received and record removed; a peer that never closes → local mode, no spawn.
3. CHANGELOG `### Changed`: a running rc.5 daemon is stopped and replaced on first contact.

Commit: `feat(mcp): retire a pre-1.0 TCP daemon on takeover`.

## Iteration 6: takeover race

**Files:** `daemon/run.rs`, `daemon/health.rs`, `daemon/spawn.rs`, `daemon/mod.rs`, `main.rs`, `engine.rs`, `daemon_tests.rs`, README.md / DEVELOPMENT.md (takeover wording), CHANGELOG.md.

1. `DaemonState` gains `Notify`; `request_shutdown` calls `notify_one()`; `run_daemon`'s `select!` gains the branch. Shutdown order: record removed → listener joined (removes the health socket) → hyperd dropped → lock dropped.
2. `maybe_take_over` and `daemon stop`: send `STOP`, wait until the lock is free (`STOP_WAIT` 15 s), then spawn / report; on timeout local mode / a clear CLI error.
3. `SPAWN_TIMEOUT` 20 s; `STARTUP_WAIT` 10 s (already used since iteration 4; confirm the value).
4. `main` logs a `run_daemon` error with `tracing::error!`; the engine's local-mode fallback logs at `warn`.
5. Fix the comments listed in spec section 4, point 6.
6. Tests per spec section 4, Tests: the takeover test (bound 30 s; red with `STARTUP_WAIT` = 0), the prompt-STOP unit test (record gone within 1 s; red with the `Notify` branch disabled), `daemon stop` returns only after the lock is free.
7. CHANGELOG `### Fixed`: version takeover starts the new daemon reliably; `daemon stop` waits for the daemon to exit; the stale-record race.

Commit: `fix(mcp): version takeover starts the new daemon reliably`.

## Phase 5

End-to-end: whole `cargo test -p hyperdb-mcp` and `make test`. Manual checks on this machine: a takeover (start a daemon with the previous build if available, otherwise two builds of this branch with an edited record version); `hyperdb-mcp daemon status` / `stop` / `doctor` against the new daemon; `kill -9` a daemon and confirm its hyperd exits and the next start succeeds (spec section 4 assumption). Then the parallel dual review (`feature-dev:code-reviewer` + `code-review`) with the reviewer brief, including the removed-surface-vs-changelog check. Update the ssteiner-ai STATUS.md (uncommitted) with the results and the deferred items (read-only attach, read-only functions, mc9-02, mc9-03, Windows integrity label, old client vs new daemon).
