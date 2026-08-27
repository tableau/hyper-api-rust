# HyperDB MCP Agent UX and Operational Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use
> `superpowers:subagent-driven-development` and execute this plan task by task.
> Steps use checkbox (`- [ ]`) syntax for tracking. The main thread owns plan
> revision, commits, final validation, and merge-readiness judgment.

**Goal:** Make HyperDB MCP installation/runtime failures diagnosable, make every
routed result identify its effective database, establish a measured 33-tool
catalog contract, and improve the built-in chart for common diagnostics without
changing defaults or breaking the published Rust structs.

**Architecture:** Add a pure diagnostics/identity layer shared by a new
side-effect-free CLI doctor and MCP status; enrich daemon wire records without
changing public `DaemonInfo`; classify SQLSTATE `55006` only at persistent
attach; centralize additive `resolved_database` response metadata; preserve the
full router while measuring its generated schema; and route new MCP chart
options through an internal presentation type so public `ChartOptions` remains
source-compatible.

**Tech stack:** Rust workspace; `hyperdb-mcp`; rmcp 1.8 generated tool router;
Clap; serde/schemars; Plotters 0.3.7; CommonJS Node wrapper; real `hyperd` via
`HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd`; Conventional Commits.

**Design specification:**
`docs/superpowers/specs/2026-08-13-hyperdb-mcp-agent-ux-design.md`

**Final integration base:** `origin/main` @ `e609061` (plan originally approved at `87e0b9d`)
**Branch:** `codex/hyperdb-mcp-agent-ux`
**Worktree:**
`/Users/ssteiner/Documents/Codex/2026-08-12/insta/hyper-api-rust-mcp-ux`

---

## Global constraints

These constraints apply to every task and every agent.

- Read and obey repository `AGENTS.md`, `CLAUDE.md`, and affected neighboring
  code before editing. Search the whole repository before concluding an API,
  test, or documentation surface is absent.
- Use `apply_patch` for edits. Preserve unrelated/user changes. Never reset,
  clean, delete, or rewrite the original checkout.
- Hyper-backed commands use exactly
  `HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd`; never invent `hyperd` flags.
- Hyper tests need local callback-listener permission in this environment. A
  sandbox `Failed to create callback listener` is environmental, not a product
  regression; rerun the identical command with loopback permission and retain
  both outputs in the evidence log.
- Do not add narrowing integer `as` casts. When a touched line already performs
  one, replace it with `TryFrom` plus an explicit saturation/error policy.
- Validate untrusted launcher JSON and chart options. Do not echo unknown
  launcher fields, environment contents, or secrets.
- Do not change the default database, default tool surface, read-only listing,
  daemon takeover comparison, public `ChartOptions`, or public `DaemonInfo`.
- Update `hyperdb-mcp/src/readme.rs` whenever a chart/tool schema changes.
- Append user-visible entries only to `hyperdb-mcp/CHANGELOG.md` under
  `## [Unreleased]`; do not edit versions or the root generated changelog.
- A command is green only when its real output and zero exit status were seen.
  No output for roughly 30 seconds is a hang/failure requiring investigation.
- Developer/tester agents do not commit. After an independent task reviewer has
  no unresolved Critical or Important finding, the main thread stages explicit
  paths and makes the task's Conventional Commit.

## Harness execution protocol

For every behavioral task:

1. A tester agent owns the named test files and proves every planned new
   assertion fails against the current branch for the intended reason. Each
   task below names its red test functions and exact command. The tester must
   capture the harness's nonzero executed-test count (or Node TAP test count)
   and the expected assertion/compiler failure; a Cargo filter that reports
   zero tests is a failed gate even when Cargo exits zero. Use `-- --exact`
   whenever one fully qualified Rust test is selected.
   When a genuinely new Rust interface makes the first red a compiler error,
   capture that nonzero compiler failure first; the tester may then add only the
   smallest `unimplemented!()` DI/signature seam allowed by the tester role and
   rerun to obtain an executed failing assertion. The engineer must replace the
   seam before any green claim.
   A table-driven test covering multiple routed tools/shapes must execute every
   case and accumulate named mismatches before its final assertion; it must not
   use `?`, `expect`, or an assertion inside the loop that short-circuits later
   red evidence.
2. An engineer agent owns the named production files and makes the proven-red
   test green with the smallest conforming change. It must not revert work from
   earlier tasks or unrelated agents.
3. The engineer runs the focused suite plus:

   ```bash
   cargo fmt --all --check
   cargo clippy -p hyperdb-mcp --all-targets --all-features -- -D warnings
   ```

4. A fresh read-only reviewer receives the relevant spec/plan sections, the
   complete task diff, and captured red/green/lint output. It reports only
   Critical / Important / Minor findings and a merge verdict.
5. Critical/Important findings return to an engineer, followed by a fresh
   re-review. The main thread independently verifies every claimed fix.
6. The main thread commits explicit paths only after the task gate is satisfied.

When a behavioral task changes README/help/tool prose, its tester adds the
semantic documentation assertion and records it red before that task edits the
prose. Task 14 tests only documentation drift deliberately deferred by Tasks
1-13; it does not retroactively claim already-green documentation assertions as
red-before-green evidence.

Compatibility characterizations are explicit exceptions to red-before-green:
Task 1's existing catalog contract,
`legacy_daemon_info_literal_is_source_compatible` in Task 3, and
`callback_connection_shutdowns_hyperd_after_parent_kill` in Task 7, and
`legacy_chart_options_literal_is_source_compatible` in Task 11 intentionally
pass on the unchanged base. They pin existing behavior/source/lifecycle
compatibility and must be recorded as passing characterizations, never
misreported as red tests.

## File map

| Area | Primary files | Tasks |
|---|---|---|
| Catalog measurement | `hyperdb-mcp/src/server.rs`, `hyperdb-mcp/tests/tool_schema_tests.rs` | 1 |
| Installation identity | `hyperdb-mcp/src/diagnostics.rs`, `src/lib.rs`, `npm/bin.js`, `npm/bin.test.js` | 2 |
| Daemon record | `src/daemon/discovery.rs`, `health.rs`, `run.rs`, `tests/daemon_tests.rs` | 3 |
| Doctor CLI | `src/main.rs`, `src/paths.rs`, `src/diagnostics.rs`, `tests/doctor_tests.rs`, README | 4 |
| Daemon routing fixes | `src/main.rs`, `src/daemon/health.rs`, `src/engine.rs`, `src/server.rs` | 5 |
| Status contract | `src/diagnostics.rs`, `src/engine.rs`, `src/server.rs`, MCP/resource tests | 6 |
| Lock classification | `src/engine.rs`, `src/error.rs`, engine/error tests | 7 |
| Routed query results | `src/server.rs`, `tests/end_to_end_mcp_tests.rs` | 8 |
| Routed data results | `src/server.rs`, per-tool/end-to-end tests | 9 |
| Routed KV/copy results | `src/server.rs`, KV/end-to-end/schema tests | 10 |
| Chart foundations | `src/chart.rs`, `src/server.rs`, `src/readme.rs`, chart/MCP tests | 11 |
| Horizontal bars | same chart surfaces | 12 |
| Log measure scale | same chart surfaces | 13 |
| Documentation sweep | README, concise README, smoke guide, demo, changelog, resource tests | 14 |
| Integrated validation/memory | whole diff, `ssteiner-ai/notes`, applicable agent profiles | 15 |

---

### Task 1: Characterize and budget the generated MCP catalog

**Owners:** tester (test), engineer only if a minimal read-only snapshot helper
is required.
**Files:**

- Create: `hyperdb-mcp/tests/tool_schema_tests.rs`
- Modify, only if needed: `hyperdb-mcp/src/server.rs`
- Reference: `hyperdb-mcp/tests/end_to_end_mcp_tests.rs:1-121`

- [ ] Reuse the in-memory rmcp duplex pattern to call `list_all_tools()` on an
      un-warmed server; do not start Hyper merely to inspect schemas.
- [ ] Assert the exact sorted legacy list of 33 names and that `doctor` is not a
      tool. Assert the full profile is unchanged by read-only mode.
- [ ] Add exactly named tests
      `generated_catalog_preserves_full_33_tool_contract`,
      `generated_catalog_budget_and_metadata_contract`, and
      `generated_catalog_readme_coverage_contract`. Serialize the typed
      `Vec<Tool>` returned by `list_all_tools()` with minified
      `serde_json::to_vec`; this canonical typed payload, excluding the JSON-RPC
      envelope, is the byte metric. Remeasure the unchanged base with that exact
      helper and calculate total bytes plus per-tool
      name/description/input-schema/other bytes. Measure initialize instructions
      and `get_readme` separately.
- [ ] Print the stable metrics only under `--nocapture` and enforce a reviewed
      high-water budget of exactly `57_344` bytes unless the unchanged-base
      measurement proves that value incorrect and the plan reviewers approve a
      replacement integer. Do not label bytes as tokens.
- [ ] Assert output-schema and annotation presence/absence for every generated
      tool rather than silently charging those fields to `other`.
- [ ] Derive README coverage from generated router names or compare generated
      names with the existing documented-name set so future tools cannot bypass
      the check by omission from a hand-maintained array.
- [ ] Run:

  ```bash
  cargo test -p hyperdb-mcp --test tool_schema_tests -- --nocapture
  cargo test -p hyperdb-mcp --test readme_tests
  ```

  Expected: PASS on unchanged behavior; output shows exactly three new catalog
  tests executed, 33 tools, and the canonical measurement emitted. Zero matching
  tests fails this characterization gate.
- [ ] Review spotlight: ensure the test measures the actual rmcp response, not a
      duplicated hand-built list; ensure server construction has no filesystem
      or engine side effects.
- [ ] Commit: `test(mcp): pin generated tool catalog contract`

### Task 2: Parse and pass installation/launcher identity

**Owners:** tester (`diagnostics_tests.rs`, Node test), engineer
(`diagnostics.rs`, `lib.rs`, `bin.js`).
**Files:**

- Create: `hyperdb-mcp/src/diagnostics.rs`
- Create: `hyperdb-mcp/tests/diagnostics_tests.rs`
- Create: `hyperdb-mcp/npm/bin.test.js`
- Modify: `hyperdb-mcp/src/lib.rs`
- Modify: `hyperdb-mcp/npm/bin.js`

- [ ] Add exactly named Rust tests `launcher_identity_parsing_contract`,
      `installation_identity_version_warning_contract`, and
      `launcher_identity_rejects_oversize_without_secret_leakage` for absent
      metadata, valid allowlisted metadata, malformed JSON, unknown-key
      ignoring, whole-value/individual-string limits, source version/build
      parsing, mismatched wrapper/platform/native versions, and a shared
      `ReportedPath { display, encoding }` created from UTF-8 and OS-supported
      non-UTF-8 paths with explicit `utf8|lossy` marking and bounded display.
      Keep parsing
      pure by accepting an `Option<&OsStr>`; do not mutate global environment
      variables in parallel tests.
- [ ] Run
      `cargo test -p hyperdb-mcp --test diagnostics_tests -- --nocapture` before
      production edits. Expected red is the cited unresolved diagnostics
      interface/compiler error; it must be a nonzero exit, never a zero-match
      Cargo pass. Once the smallest compile seam exists, rerun the same command
      before implementing parsing and capture three executed failing tests.
- [ ] Before editing `bin.js`, create Node tests named
      `launcher_module_is_import_safe`,
      `launcher_info_contains_only_allowlisted_fields`,
      `launcher_preserves_spawn_error_semantics`,
      `launcher_preserves_numeric_exit_status`, and
      `launcher_preserves_signal_termination`. Use a child process for the
      import-safety red assertion so the current top-level `process.exit`
      cannot kill the test runner, and dependency-inject `spawnSync` after the
      refactor. Run `node --test hyperdb-mcp/npm/bin.test.js`; expected nonzero
      TAP with all five named tests discovered and the import/export contract
      failing for the intended reason.
- [ ] Refactor `bin.js` behind `main()` and `if (require.main === module)` so a
      pure launcher-info builder can be exported to `node:test` without spawning
      the native binary. Preserve shebang, argument forwarding, inherited stdio,
      exit status, platform resolution, and bundled `HYPERD_PATH` behavior.
- [ ] Pass a single private `HYPERDB_MCP_LAUNCHER_INFO` JSON value containing
      only wrapper package name/version/path, platform package
      name/version/path, and selected executable path. Source manifests may
      yield `null` versions; never guess them.
- [ ] In Rust, add serializable `ReportedPath`, `InstallationIdentity`,
      `LauncherIdentity`, and warning types. `ReportedPath` is the one bounded,
      encoding-aware representation used by installation, configuration, and
      daemon reporting in later tasks. Cap launcher JSON at 16 KiB and each
      reported string at 4 KiB, parse only known fields, label it
      launcher-reported, and use `current_exe`, `mcp_version_string`, and
      `hyper_api_version_string` as authoritative native facts.
- [ ] Compare semver bases without treating the `.r<hash>` build suffix as npm
      semver. Malformed values warn; they do not crash MCP startup.
- [ ] Run:

  ```bash
  cargo test -p hyperdb-mcp --test diagnostics_tests
  node --test hyperdb-mcp/npm/bin.test.js
  node --check hyperdb-mcp/npm/bin.js
  ```

  Expected: all PASS; Node tests prove metadata composition and that imported
  `bin.js` does not execute `main()`. They also deterministically prove spawn
  errors, numeric statuses, and signal termination preserve existing wrapper
  exit behavior.
- [ ] Review spotlight: environment injection, secret/unknown-field leakage,
      Windows path/package behavior, and preservation of wrapper exit semantics.
- [ ] Commit: `fix(mcp): report npm launcher identity`

### Task 3: Enrich daemon discovery without changing `DaemonInfo`

**Owners:** tester (`daemon_tests.rs` plus module tests), engineer (daemon
modules).
**Files:**

- Modify: `hyperdb-mcp/src/daemon/discovery.rs`
- Modify: `hyperdb-mcp/src/daemon/health.rs`
- Modify: `hyperdb-mcp/src/daemon/run.rs`
- Modify: `hyperdb-mcp/tests/daemon_tests.rs`

- [ ] Add external `legacy_daemon_info_literal_is_source_compatible`; add private
      module tests
      `daemon::discovery::tests::daemon_record_old_and_new_flat_wire_contract`,
      `daemon::discovery::tests::raw_discovery_read_is_non_mutating_and_distinguishes_io`,
      and `daemon::health::tests::health_status_returns_flat_enriched_record`.
      Private wire/reader types stay private; no test-only public API is added.
      The tests deserialize old flat
      JSON, round-trip new build/executable identity, prove old `DaemonInfo`
      readers ignore the additive object, compile an exhaustive legacy literal,
      and prove a raw read never deletes stale/malformed/unreadable discovery
      state. `NotFound` is missing; permission/other I/O is unreadable.
- [ ] Run the three module tests with
      `cargo test -p hyperdb-mcp --lib <fully-qualified-name> -- --exact --nocapture`
      and the external literal test with
      `cargo test -p hyperdb-mcp --test daemon_tests legacy_daemon_info_literal_is_source_compatible -- --exact --nocapture`.
      Expected one executed red assertion per behavioral module command after
      the allowed compile seam, while the legacy literal characterization passes
      on the base. The richer record/raw inspection behavior does not yet exist;
      zero matching tests fails the gate.
- [ ] Add a separate version-tolerant record with exact shape
      `#[serde(flatten)] info: DaemonInfo` plus one optional additive `identity`
      object containing `DaemonBuildIdentity`. Legacy fields remain top-level;
      never emit a nested `info` object. Keep every field of public `DaemonInfo`
      exactly unchanged. Represent paths through the shared encoding-aware path
      type without assuming UTF-8.
- [ ] Preserve `write_discovery_file(&DaemonInfo)` for compatibility. Add an
      enriched writer for daemon runtime and update every initial/restart write
      site. Old files parse with absent identity.
- [ ] Make health `STATUS` serialize that same flat enriched record while leaving its
      shared `Arc<Mutex<DaemonInfo>>` state and public signatures intact; existing
      readers continue to ignore the extra object.
- [ ] Add a raw non-mutating reader with distinguishable missing, unreadable,
      malformed, and parsed outcomes. Do not change cleanup behavior of normal
      `discover()`.
- [ ] Run:

  ```bash
  cargo test -p hyperdb-mcp --lib daemon::discovery::tests::daemon_record_old_and_new_flat_wire_contract -- --exact --nocapture
  cargo test -p hyperdb-mcp --lib daemon::discovery::tests::raw_discovery_read_is_non_mutating_and_distinguishes_io -- --exact --nocapture
  cargo test -p hyperdb-mcp --lib daemon::health::tests::health_status_returns_flat_enriched_record -- --exact --nocapture
  cargo test -p hyperdb-mcp --test daemon_tests legacy_daemon_info_literal_is_source_compatible -- --exact --nocapture
  ```

  Expected: PASS, including old-schema compatibility and no stale-file deletion.
- [ ] Review spotlight: exact flat fixtures, exhaustive-literal compatibility,
      takeover semver, both initial and restart write sites, serde
      forward/backward behavior, and path privacy.
- [ ] Commit: `fix(mcp): enrich daemon discovery identity`

### Task 4: Add side-effect-free doctor report and CLI

**Owners:** tester (`doctor_tests.rs`, diagnostics unit tests, README test),
engineer (diagnostics/path/CLI).
**Files:**

- Create: `hyperdb-mcp/tests/doctor_tests.rs`
- Modify: `hyperdb-mcp/src/diagnostics.rs`
- Modify: `hyperdb-mcp/src/paths.rs`
- Modify: `hyperdb-mcp/src/main.rs`
- Modify: `hyperdb-mcp/src/server.rs` only for a crate-private catalog snapshot
- Modify: `hyperdb-mcp/README.md` (minimal doctor usage)
- Modify: `hyperdb-mcp/tests/readme_tests.rs` (minimal doctor contract)

- [ ] Add pure unit tests
      `diagnostics::tests::collect_doctor_state_matrix_is_pure` and
      `diagnostics::tests::candidates_refetch_and_verify_enriched_status` around a
      collector with injected raw-reader, status-prober, bounded-scanner, and
      clock/deadline functions. Drive missing, unreadable, malformed,
      parsed-unreachable, live-from-discovery, and live-from-scan states without
      real fixed ports. The collector's dependency bundle exposes no writer or
      cleanup operation. A raw discovery record and a scan result are both only
      candidate locations: neither is live identity evidence until a fresh
      enriched `STATUS` response is parsed and its health port matches the
      responding candidate. Use fresh facts, compare a discovery candidate with
      its raw PID/build/executable, and add a deterministic mismatch case that
      emits a stale/replaced warning without deleting the file.
- [ ] Add child-process tests `doctor_cli_json_and_human_smoke_is_side_effect_free`
      and `doctor_human_output_escapes_and_bounds_reported_paths` using
      `env!("CARGO_BIN_EXE_hyperdb-mcp")` and isolated temp values for state,
      persistent path, HOME/USERPROFILE, and launcher metadata. Assert matching
      JSON/human facts and byte-for-byte absence of newly created state
      directories, discovery files, persistent files, logs, or scratch
      databases. Include C0/ESC in a known field, a non-UTF-8 path where the OS
      supports it, an overlong value, and an unknown secret sentinel. Human
      output escapes controls; every path has `display` plus `utf8|lossy`; the
      sentinel is absent; the report warns that local paths need review before
      sharing.
- [ ] Add `doctor_readme_contract` before editing README and require its red
      omission to mention exact CLI spelling and side-effect-free scope.
- [ ] Run these exact red commands:

  ```bash
  cargo test -p hyperdb-mcp --lib diagnostics::tests::collect_doctor_state_matrix_is_pure -- --exact --nocapture
  cargo test -p hyperdb-mcp --lib diagnostics::tests::candidates_refetch_and_verify_enriched_status -- --exact --nocapture
  cargo test -p hyperdb-mcp --test doctor_tests -- --nocapture
  cargo test -p hyperdb-mcp --test readme_tests doctor_readme_contract -- --exact --nocapture
  ```

  Expected nonzero failures for the missing collector/subcommand/prose, with the
  two named child tests and README test discovered once their compile seams
  exist. Missing daemon/default persistent file remain informational, not an
  automatic command failure. Zero matching tests fails the gate.
- [ ] Preserve `resolve_persistent_db_path` and add source-aware resolution used
      by CLI/doctor (`cli`, deprecated alias, environment, platform default,
      disabled). Keep existing precedence and conflict errors.
- [ ] Implement the typed report sections from the spec: `status`,
      `installation`, `configuration`, `daemon`, `tool_catalog`, `warnings`.
      Catalog measurement must use the generated router snapshot from Task 1.
- [ ] Consume Task 2's bounded `ReportedPath { display, encoding }` for every
      configuration path (installation and daemon paths already use it). Escape
      C0/DEL/ESC in all human-rendered untrusted values, and retain serde JSON
      escaping in JSON mode.
- [ ] Report observed `HYPERD_PATH` and the documented upward
      `.hyperd/current/hyperd` candidate without starting Hyper or inventing PATH
      search behavior.
- [ ] Add `Commands::Doctor { json: bool }`; restructure command extraction so
      remaining global CLI fields can be inspected without a partial-move bug,
      then return before logging and engine paths. Exit zero when a report is
      produced even with warnings, including unreadable discovery.
- [ ] Run:

  ```bash
  cargo test -p hyperdb-mcp --lib diagnostics::tests::collect_doctor_state_matrix_is_pure -- --exact --nocapture
  cargo test -p hyperdb-mcp --lib diagnostics::tests::candidates_refetch_and_verify_enriched_status -- --exact --nocapture
  cargo test -p hyperdb-mcp --test doctor_tests -- --nocapture
  cargo test -p hyperdb-mcp --test diagnostics_tests
  cargo test -p hyperdb-mcp --test tool_schema_tests -- --nocapture
  cargo test -p hyperdb-mcp --test readme_tests doctor_readme_contract -- --exact --nocapture
  cargo run -p hyperdb-mcp -- doctor --json
  ```

  Expected: all tests PASS; manual JSON is valid and does not start `hyperd`.
- [ ] Review spotlight: every filesystem mutation path, scan time bounds,
      truthful path-source labels, exit semantics, and tool count remaining 33.
- [ ] Commit: `fix(mcp): add side-effect-free doctor command`

### Task 5: Route daemon control messages to the effective health port

**Owners:** tester (`daemon_tests.rs`, `recovery_tests.rs`), engineer
(CLI/health/engine/server).
**Files:**

- Modify: `hyperdb-mcp/src/main.rs`
- Modify: `hyperdb-mcp/src/daemon/health.rs`
- Modify: `hyperdb-mcp/src/engine.rs`
- Modify: `hyperdb-mcp/src/server.rs`
- Modify: `hyperdb-mcp/tests/daemon_tests.rs`
- Modify: `hyperdb-mcp/tests/recovery_tests.rs`

- [ ] Add exactly named child/helper tests
      `daemon_status_post_action_port_targets_explicit_listener`,
      `report_hyperd_error_targets_discovered_health_port`, and
      `slow_health_report_does_not_hold_engine_mutex`. The first invokes the
      literal process spelling `hyperdb-mcp daemon status --port <N>` against an
      isolated listener and proves N is used without discovery. The second uses
      a scanned non-base daemon and proves `REPORT_HYPERD_ERROR` reaches its
      health port. The third makes the report endpoint slow and proves, within a
      channel timeout, that another engine/status caller acquires the engine
      mutex before the bounded socket report completes.
- [ ] Run each test by full name with `-- --exact --nocapture`; expected red on
      ignored post-action CLI port, base-port reporting, and/or I/O while the
      guard is held. Each command must show one executed test and the intended
      assertion failure.
- [ ] Change `daemon_status` to accept `Option<u16>` and honor an explicit port;
      use discovery/scan only when absent. Make `port` a compatible Clap global
      daemon argument (or equivalent) so both existing
      `daemon --port <N> status` and required `daemon status --port <N>` parse;
      do not silently change start/stop syntax.
- [ ] Change the report helper to take the effective health port. In
      `try_daemon_mode`, pass `info.health_port`. In `with_engine`, capture
      `engine.daemon_health_port()` before running the closure. Put the mutex
      guard in an explicit inner scope/drop it, then perform heartbeat or
      loss-report TCP I/O. Never perform health-network I/O while the guard is
      held or relock merely to recover the port.
- [ ] Apply finite connect/read/write timeouts to every best-effort health
      report path; timeout remains a logged/best-effort failure.
- [ ] Preserve best-effort/no-panic semantics and skip reports in local mode.
- [ ] Run:

  ```bash
  cargo test -p hyperdb-mcp --test daemon_tests daemon_status_post_action_port_targets_explicit_listener -- --exact --nocapture
  cargo test -p hyperdb-mcp --test daemon_tests report_hyperd_error_targets_discovered_health_port -- --exact --nocapture
  cargo test -p hyperdb-mcp --test recovery_tests slow_health_report_does_not_hold_engine_mutex -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test daemon_tests -- --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test recovery_tests
  ```

  Expected: PASS with local callback permission.
- [ ] Review spotlight: explicit guard lifetime, bounded network time, deadlock
      risk, `Option<u16>` handling, fallback/local behavior, and explicit port
      not accidentally invoking cleanup discovery.
- [ ] Commit: `fix(mcp): target effective daemon health port`

### Task 6: Unify installation identity and document degraded status

**Owners:** tester (end-to-end MCP, resource, and README tests), engineer
(diagnostics/status).
**Files:**

- Modify: `hyperdb-mcp/src/diagnostics.rs`
- Modify: `hyperdb-mcp/src/engine.rs`
- Modify: `hyperdb-mcp/src/server.rs`
- Modify: `hyperdb-mcp/tests/end_to_end_mcp_tests.rs`
- Modify: `hyperdb-mcp/tests/resource_tests.rs`
- Modify: `hyperdb-mcp/tests/readme_tests.rs`
- Modify: `hyperdb-mcp/src/readme.rs` (status contract)

- [ ] Extend the MCP harness to retain `server.engine_handle()`. Use a native
      thread plus ready/release channels to hold the `std::sync::Mutex` without
      carrying a non-Send guard across `.await`.
- [ ] Add exactly named MCP tests
      `status_full_and_degraded_share_identity_contract` and
      `status_degraded_returns_promptly_while_engine_locked` for `mcp_version`, correct
      `hyper_rust_api_version`, `installation`, `default_database: "local"`,
      `engine_busy`, intentional omissions, and prompt return while locked.
- [ ] Add `resource_status_renderer_uses_actual_engine_keys`. The resource
      consumes only actual `has_persistent`/`persistent_path` values from
      `Engine::status`; because the engine emits no `read_only` key, render that
      fact directly from `HyperMcpServer::read_only` (the same source already
      used to augment full/degraded status). Do not invent or look up a new
      engine-status key. Remove obsolete workspace-key lookups.
- [ ] Add `readme_degraded_status_contract` before editing `src/readme.rs`; it
      asserts partial/non-definitive semantics and retry guidance.
- [ ] Run each of the four tests by full name with `-- --exact --nocapture`.
      Expected one executed failing test per command because identities/default
      field/prose are absent, the degraded API identity is mislabeled, and the
      resource consumes stale keys. Zero matches fails the gate.
- [ ] Add one response augmentation path used by both full and degraded status.
      Preserve all existing engine statistics/root fields.
- [ ] Fix degraded API version and update the tool/concise README wording:
      `engine_busy: true` is partial; degraded `hyperd_running: false` may be
      inconclusive; retry for full statistics.
- [ ] Correct the workspace/readme renderer to consume actual status keys.
- [ ] Run:

  ```bash
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests status_full_and_degraded_share_identity_contract -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests status_degraded_returns_promptly_while_engine_locked -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test resource_tests resource_status_renderer_uses_actual_engine_keys -- --exact --nocapture
  cargo test -p hyperdb-mcp --test readme_tests readme_degraded_status_contract -- --exact --nocapture
  ```

  Expected: PASS; degraded call completes within the test's explicit bound.
- [ ] Review spotlight: mutex/thread determinism, status compatibility, no
      expensive doctor probes in every status call, and identity consistency.
- [ ] Commit: `fix(mcp): unify full and degraded status identity`

### Task 7: Classify persistent attachment contention contextually

**Owners:** tester (engine module, `engine_tests.rs`, `error_tests.rs`,
end-to-end MCP tests, and Hyper API process lifecycle test), engineer
(`engine.rs`, `error.rs`).
**Files:**

- Modify: `hyperdb-mcp/src/engine.rs`
- Modify: `hyperdb-mcp/src/error.rs`
- Modify: `hyperdb-mcp/tests/engine_tests.rs`
- Modify: `hyperdb-mcp/tests/error_tests.rs`
- Modify: `hyperdb-mcp/tests/end_to_end_mcp_tests.rs`
- Modify: `hyperdb-api/tests/process_tests.rs` (parent-death characterization)

- [ ] Add private module test
      `engine::tests::persistent_attach_55006_maps_resource_busy`, a synthetic
      failing test for a structured Hyper server error with
      SQLSTATE `55006` in persistent-attach context. Assert `RESOURCE_BUSY`, the
      effective path, `(55006)`/raw message, doctor guidance, and non-accusatory
      possible-owner wording.
- [ ] Add `real_persistent_lock_reproduces_resource_busy`: keep one no-daemon Engine alive with a persistent
      file, attempt a second private Engine against the same file, and assert the
      same classification. Run the entire potentially blocking reproduction in
      a dedicated child instance of the integration-test binary selected by an
      environment sentinel and exact helper-test name. The parent owns that
      exact child handle; on timeout it kills and waits for the child before
      failing, and on normal exit it waits and checks output/status. Parent and
      child use RAII temp paths. A channel timeout around an in-process worker is
      insufficient because it cannot unwind or join a blocked attach. Do not add
      `hyperd` flags or sleeps.
- [ ] Before relying on child containment, add passing characterization
      `callback_connection_shutdowns_hyperd_after_parent_kill` to the Hyper API
      process tests. A helper child creates `HyperProcess`, reports its exact
      public `pid()`, and blocks; the parent kills and waits for that helper,
      then bounded-polls the exact reported hyperd PID until it exits through
      the callback-connection dead-man switch. If the characterization fails,
      do not proceed or claim cleanup: design and independently review exact
      process-group/job-object containment or a lifecycle fix first.
- [ ] Add `non_attach_55006_preserves_existing_mapping`, proving the global
      `From<hyperdb_api::Error>` mapper does not blindly call it a database lock.
- [ ] Add MCP-level `persistent_lock_keeps_mcp_available`: hold the file with a
      private engine, warm a server against it, assert status remains promptly
      usable, then assert the first persistent-routed query returns structured
      `RESOURCE_BUSY` with the same evidence. Contain the complete scenario in
      the same parent-controlled child-process pattern; every timeout path kills
      and waits for that exact child before the parent test returns.
- [ ] Run all four tests by full name with `-- --exact --nocapture`. Expected
      one executed failing test per applicable command because attachment is
      currently `INTERNAL_ERROR` and the MCP-level contract is absent. Zero
      matches or a timeout is a failed gate.
- [ ] Add a private persistent-attach conversion helper and use it only around
      create/attach of the reserved persistent database. Preserve raw error and
      SQLSTATE. Retain phrase fallback for older Hyper messages.
- [ ] Improve generic `RESOURCE_BUSY` guidance and correct `HYPERD_PATH` advice
      that currently claims arbitrary PATH search.
- [ ] Run:

  ```bash
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-api --test process_tests callback_connection_shutdowns_hyperd_after_parent_kill -- --exact --nocapture
  cargo test -p hyperdb-mcp --lib engine::tests::persistent_attach_55006_maps_resource_busy -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test engine_tests real_persistent_lock_reproduces_resource_busy -- --exact --nocapture
  cargo test -p hyperdb-mcp --test error_tests non_attach_55006_preserves_existing_mapping -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests persistent_lock_keeps_mcp_available -- --exact --nocapture
  ```

  Expected: PASS; real reproduction finishes without hanging.
- [ ] Review spotlight: global `55006` boundary, Windows ingest behavior, error
      ownership claims, create-vs-attach context, and raw diagnostic retention.
- [ ] Commit: `fix(mcp): classify persistent database contention`

### Task 8: Add `resolved_database` to query-oriented results

**Owners:** tester (`end_to_end_mcp_tests.rs`), engineer (`server.rs`).
**Files:**

- Modify: `hyperdb-mcp/src/server.rs`
- Modify: `hyperdb-mcp/tests/end_to_end_mcp_tests.rs`

- [ ] Add exactly named MCP test `resolved_database_query_success_shapes` for
      `query`, `execute`, `sample`, `describe`, and `chart`, covering default
      local plus representative persistent/mixed-case/attached routes. Before
      writing assertions, inventory every successful return branch in these
      handlers. Exercise normal, zero-row/empty, and custom content shapes where
      those are currently successes; do not turn an existing error into success
      to manufacture coverage. Query assertions parse its second text block;
      chart assertions preserve image-first/text-stats delivery.
- [ ] Pin every pre-existing field, structured/text mirroring, and content order
      for each branch at the same time.
- [ ] Implement the table as non-short-circuiting case aggregation: record tool
      call errors and field/order mismatches by case name, execute all query
      shapes, then fail once with the complete mismatch list. The first missing
      field must not hide later red evidence.
- [ ] Run
      `HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests resolved_database_query_success_shapes -- --exact --nocapture`;
      expected one executed failing test solely because `resolved_database` is
      absent. Zero matches fails the gate.
- [ ] Add small helpers near `resolve_db` that canonicalize `None` to `local`
      and inject a top-level field into object responses. Do not put routing in
      generic stats structs.
- [ ] Thread the resolved name through each custom response builder. Preserve
      query SQL formatting, chart image delivery, and structured/text mirroring.
- [ ] Replace touched integer `as` conversions with explicit `TryFrom` policy.
- [ ] Run:

  ```bash
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests resolved_database_query_success_shapes -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test sample_tests
  ```

  Expected: PASS for all five tool families.
- [ ] Review spotlight: database precedence, alias lowercasing, special response
      shapes, strict old clients, and helper failure on non-object JSON.
- [ ] Commit: `fix(mcp): report resolved database for query tools`

### Task 9: Add `resolved_database` to ingest/export/watch/metadata results

**Owners:** tester (per-tool/end-to-end tests), engineer (`server.rs`).
**Files:**

- Modify: `hyperdb-mcp/src/server.rs`
- Modify: `hyperdb-mcp/tests/end_to_end_mcp_tests.rs`
- Modify: `hyperdb-mcp/tests/per_tool_database_tests.rs` where useful

- [ ] Add exactly named test `resolved_database_data_success_shapes` with response assertions for `load_data`, `load_file`,
      `load_files`, `watch_directory`, `export`, and `set_table_metadata`.
      Include local, `persist: true`, explicit local winning over persist, and a
      canonical attached alias across the group.
- [ ] Inventory and exercise every existing successful shape across these
      handlers, including empty, not-found/idempotent, and partial per-file
      shapes where the implementation currently treats them as success. Pin all
      prior fields and notifications. Do not redefine an error as success merely
      to satisfy the matrix.
- [ ] Aggregate every named data-tool case without `?`/`expect`/loop assertions,
      then fail once with all mismatches so the captured red output proves every
      planned shape was reached.
- [ ] Reuse temp files/directories and existing watcher cleanup; do not add
      nondeterministic sleeps when registry state can be observed directly.
- [ ] Run
      `HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests resolved_database_data_success_shapes -- --exact --nocapture`;
      expected one executed failing test on missing result metadata. Zero
      matches fails the gate.
- [ ] Inject the common top-level field after successful routing. For
      `load_files`, one top-level target is authoritative because all entries
      share it; do not duplicate it into every per-file result.
- [ ] Preserve watcher handles, export stats/path, catalog updates, and resource
      notifications.
- [ ] Run:

  ```bash
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests resolved_database_data_success_shapes -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test per_tool_database_tests
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test watcher_tests
  ```

  Expected: PASS.
- [ ] Review spotlight: parallel `load_files` top-level truth, watcher teardown,
      output-path behavior, and catalog routing side effects.
- [ ] Commit: `fix(mcp): report resolved database for data tools`

### Task 10: Complete routed metadata for KV and copy tools

**Owners:** tester (KV/end-to-end/schema tests), engineer (`server.rs`).
**Files:**

- Modify: `hyperdb-mcp/src/server.rs`
- Modify: `hyperdb-mcp/tests/kv_tools_tests.rs`
- Modify: `hyperdb-mcp/tests/end_to_end_mcp_tests.rs`
- Modify: `hyperdb-mcp/tests/tool_schema_tests.rs`

- [ ] Add exactly named `resolved_database_kv_success_shapes` for all nine KV
      tools. Verify every normal and currently-successful missing-key/empty-store
      shape carries the canonical target while preserving prior fields.
- [ ] Aggregate all nine KV tools and their named success branches before one
      final assertion; no early failure may conceal a later missing field.
- [ ] Add `copy_query_preserves_target_and_resolved_database`; assert its legacy
      `target_database` remains present and equals the new common field in every
      success shape.
- [ ] Add `routed_tool_allowlist_matches_generated_schemas` with an explicit
      21-tool semantic allowlist from the design. Compare it with generated
      schema candidates exposing `database` and/or `persist`, then handle
      `copy_query` as the named semantic exception because it exposes
      `target_database`. Property-name detection alone must not define routing
      semantics or prove response injection.
- [ ] Run the three tests by full name with `-- --exact --nocapture`; expected
      one executed failing test per command on metadata and/or inventory
      coverage. Zero matches fails the gate.
- [ ] Return/thread the canonical resolved name out of each KV engine closure
      alongside its value, then apply the common helper to KV/copy success
      values only. Never reconstruct the target from the original request after
      resolution. Do not alter read-only guards, overwrite semantics, or current
      attached-read behavior.
- [ ] Run:

  ```bash
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test kv_tools_tests resolved_database_kv_success_shapes -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests copy_query_preserves_target_and_resolved_database -- --exact --nocapture
  cargo test -p hyperdb-mcp --test tool_schema_tests routed_tool_allowlist_matches_generated_schemas -- --exact --nocapture
  ```

  Expected: PASS; schema inventory equals the documented routed inventory.
- [ ] Review spotlight: every success branch including `found:false`, copy
      compatibility, no metadata on errors, and schema inventory false positives
      such as `target_database`-only tools.
- [ ] Commit: `fix(mcp): complete resolved database result metadata`

### Task 11: Preserve chart API while fixing range/category behavior

**Owners:** tester (chart unit/integration/MCP tests), engineer (chart).
**Files:**

- Modify: `hyperdb-mcp/src/chart.rs`
- Modify: `hyperdb-mcp/tests/chart_tests.rs`
- Modify: `hyperdb-mcp/tests/end_to_end_mcp_tests.rs`

- [ ] Add passing characterization
      `legacy_chart_options_literal_is_source_compatible`, constructing public
      `ChartOptions` with exactly its legacy fields and calling the public
      `render_chart` signature. This is a compatibility characterization, not a
      fabricated red test.
- [ ] In `chart.rs`'s private unit-test module add failing
      `chart::tests::bar_ranges_and_categories_are_validated`, covering numeric
      x plus `x_as_category:false`, applied bar `y_range`, positive-only and
      negative-only linear baselines, and reversed/equal/non-finite explicit
      ranges. Add MCP-level `chart_mcp_rejects_invalid_ranges` for structured
      `INVALID_ARGUMENT` mapping.
- [ ] Run the compile characterization by full name and expect one pass. Then
      run each new behavior test by full name with `-- --exact --nocapture` and
      expect one executed failure on ignored range/off-canvas behavior or wrong
      error mapping. Zero matches fails the gate.
- [ ] Keep public `ChartOptions` and `render_chart` signatures/fields exactly
      unchanged; Task 11 adds no MCP fields or presentation type.
- [ ] Treat bar x values categorically regardless of `x_as_category:false`,
      apply `y_range`, and validate every explicit range as finite and strictly
      increasing before Plotters.
- [ ] Use zero as the linear bar baseline when the range includes it; otherwise
      use the nearer explicit boundary (lower for positive-only, upper for
      negative-only). Return `INVALID_ARGUMENT` for caller-invalid ranges.
      Preserve legacy vertical/linear/default-legend rendering.
- [ ] Run:

  ```bash
  cargo test -p hyperdb-mcp --test chart_tests legacy_chart_options_literal_is_source_compatible -- --exact --nocapture
  cargo test -p hyperdb-mcp --lib chart::tests::bar_ranges_and_categories_are_validated -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests chart_mcp_rejects_invalid_ranges -- --exact --nocapture
  cargo test -p hyperdb-mcp --test chart_tests -- --nocapture
  ```

  Expected: PASS; public source compatibility and existing chart regressions
  remain green.
- [ ] Review spotlight: public Rust source compatibility, fixed-range semantics,
      categorical positioning, negative bars, and no physical/data-axis
      confusion.
- [ ] Commit: `fix(mcp): validate chart ranges and bar categories`

### Task 12: Add horizontal bars, legend control, and value labels

**Owners:** tester (chart/MCP tests), engineer (chart/server/readme).
**Files:**

- Modify: `hyperdb-mcp/src/chart.rs`
- Modify: `hyperdb-mcp/src/server.rs`
- Modify: `hyperdb-mcp/src/readme.rs`
- Modify: `hyperdb-mcp/tests/chart_tests.rs`
- Modify: `hyperdb-mcp/tests/end_to_end_mcp_tests.rs`
- Modify: `hyperdb-mcp/tests/readme_tests.rs`

- [ ] In `chart.rs`'s private unit-test module add failing
      `chart::tests::horizontal_bar_layout_contract` and
      `chart::tests::legend_and_value_label_contract`. They test pure
      category/group/layout output plus SVG geometry: first SQL category at the
      top, distinct grouped rectangles, swapped descriptions, legend
      suppression, original scalar values, Unicode, and a PNG smoke path.
- [ ] Characterize existing vertical behavior first and make horizontal match
      it: one category is supported; duplicate category+series rows remain
      distinct overlapping marks in input order; missing series/category cells
      remain gaps; series retain deterministic existing ordering; the eight
      colors cycle for later series; and long/Unicode labels are accepted but
      neither truncated nor auto-sized. These are explicit supported outcomes,
      even if a caller must increase width/height to avoid clipping.
- [ ] Add MCP test `chart_mcp_presentation_options_contract` for accepted schema,
      defaults, invalid cross-chart combinations, result content order, and PNG
      delivery. Add `readme_chart_presentation_contract` before prose edits for
      all three new controls and the layout caveat.
- [ ] Run all four tests by full name with `-- --exact --nocapture`; expected one
      executed red test per command because fields/rendering/docs are absent.
      The private renderer behavior tests live inside `chart.rs`; external tests
      must not expose an internal type merely to obtain a seam.
- [ ] Add optional MCP `bar_orientation`, `label_values`, and `show_legend`
      fields plus an internal typed presentation-options value and extended
      renderer. Public `render_chart` delegates with legacy defaults. Reject
      `label_values:true` for non-bars and explicit bar orientation on non-bars
      as `INVALID_ARGUMENT`.
- [ ] Default `show_legend` to true. `false` suppresses bar/line/scatter legends;
      existing `label_points:true` still suppresses line/scatter legends
      regardless of this flag.
- [ ] Reuse grouping/order/color logic, reverse the categorical coordinate so
      the first query row is at the top, and increase horizontal category-label
      area without adding auto-sizing.
- [ ] Extend the internal point model to retain original y scalar text before
      numeric conversion; render that exact text for value labels without a
      formatting DSL or collision solver. Keep public chart API unchanged.
- [ ] Only after the documentation test is red, update the concise README/tool
      description in the same task.
- [ ] Run:

  ```bash
  cargo test -p hyperdb-mcp --lib chart::tests::horizontal_bar_layout_contract -- --exact --nocapture
  cargo test -p hyperdb-mcp --lib chart::tests::legend_and_value_label_contract -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests chart_mcp_presentation_options_contract -- --exact --nocapture
  cargo test -p hyperdb-mcp --test readme_tests readme_chart_presentation_contract -- --exact --nocapture
  cargo test -p hyperdb-mcp --test chart_tests -- --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests chart
  cargo test -p hyperdb-mcp --test tool_schema_tests -- --nocapture
  ```

  Expected: PASS for PNG/SVG and schema budget.
- [ ] Review spotlight: top-to-bottom order, negative linear bars, grouped
      offsets, label clipping, backend parity, and invalid option combinations.
- [ ] Commit: `fix(mcp): add diagnostic chart presentation controls`

### Task 13: Add positive logarithmic measure scale

**Owners:** tester (chart/MCP tests), engineer (chart/server/readme).
**Files:** same as Task 12.

- [ ] In `chart.rs`'s private unit-test module add failing
      `chart::tests::log_range_handles_finite_extremes` and
      `chart::tests::log_rendering_contract`. Cover positive bar/line/scatter,
      a repeated value, minimum positive subnormal, maximum finite, explicit
      positive range, vertical/horizontal SVG and PNG, and rejection of zero,
      negative, mixed-sign, non-finite, reversed/non-increasing, histogram, or
      explicit ranges that do not contain every plotted value. Verify no bar
      starts at numeric zero or produces inverted geometry.
- [ ] Add MCP `chart_mcp_log_scale_contract` for parser/error/content behavior
      and `readme_chart_log_contract` before prose edits.
- [ ] Run all four tests by full name with `-- --exact --nocapture`; expected one
      executed red test per command because scale/helper/docs are absent. Keep
      internal renderer behavior tests inside `chart.rs`.
- [ ] Add internal typed measure scale and optional MCP `y_scale`, defaulting to
      linear. Keep semantics tied to data-role y even for horizontal bars.
- [ ] Validate all values/ranges before building Plotters contexts. Use the
      verified Plotters `.log_scale()` API. Prefer small explicit linear/log
      branches over complex generic abstraction because their `ChartContext`
      coordinate types differ.
- [ ] Compute automatic bounds in natural-log space with five-percent span
      padding. Clamp log endpoints to
      `ln(f64::from_bits(1))..=ln(f64::MAX)` before exponentiation. A repeated
      value uses a fixed five-percent decade span. If rounding collapses an
      endpoint at a bound, use the adjacent representable positive float on the
      available side. The final finite positive increasing range must enclose
      all values. An explicit log range must contain every plotted value.
- [ ] Bars start at the effective positive lower bound, never zero. Do not add
      x-log, symlog, histogram log, negative-only log, value clamping, or silent
      filtering.
- [ ] Only after the documentation test is red, update concise README and MCP
      description in the same task.
- [ ] Run:

  ```bash
  cargo test -p hyperdb-mcp --lib chart::tests::log_range_handles_finite_extremes -- --exact --nocapture
  cargo test -p hyperdb-mcp --lib chart::tests::log_rendering_contract -- --exact --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests chart_mcp_log_scale_contract -- --exact --nocapture
  cargo test -p hyperdb-mcp --test readme_tests readme_chart_log_contract -- --exact --nocapture
  cargo test -p hyperdb-mcp --test chart_tests -- --nocapture
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test end_to_end_mcp_tests chart
  cargo test -p hyperdb-mcp --test tool_schema_tests -- --nocapture
  ```

  Expected: PASS; all invalid values fail as `INVALID_ARGUMENT` before backend
  rendering.
- [ ] Review spotlight: subnormal/huge floats, auto-range equality, horizontal
      data-axis mapping, generic code complexity, and silent misrepresentation.
- [ ] Commit: `fix(mcp): add positive logarithmic chart scale`

### Task 14: Align public documentation, terminology, and changelog

**Owners:** tester (`doctor_tests.rs` plus semantic documentation assertions),
doc-editor agent (prose), followed by reviewer.
**Files:**

- Modify: `hyperdb-mcp/src/readme.rs`
- Modify: `hyperdb-mcp/README.md`
- Modify: `hyperdb-mcp/SMOKE_TESTS.md`
- Modify: `hyperdb-mcp/examples/demo.rs`
- Modify: `hyperdb-mcp/src/main.rs` help text
- Modify: `hyperdb-mcp/src/server.rs` tool/parameter descriptions
- Modify: `hyperdb-mcp/CHANGELOG.md`
- Modify: `hyperdb-mcp/tests/readme_tests.rs`
- Modify: `hyperdb-mcp/tests/resource_tests.rs`
- Modify: `hyperdb-mcp/tests/doctor_tests.rs`

- [ ] First rerun the already-green semantic contracts added in Tasks 4, 6, 12,
      and 13 for doctor, full/degraded status, and chart controls. Do not present
      these as Task 14 red evidence.
- [ ] Before Task 14 prose edits, add exactly named deferred-drift tests
      `public_docs_database_and_read_only_contract`,
      `smoke_demo_and_changelog_contract`, and
      `cli_help_matches_hyperd_and_read_only_contract`. They cover
      `RESOURCE_BUSY`, `resolved_database`, actual guarded/allowed read-only
      tools, local/persistent/attached terminology, `HYPERD_PATH` resolution,
      smoke/demo truth, and the crate `## [Unreleased]` contract. Run each by
      full name with `-- --exact --nocapture`; expected one executed failure per
      command on the cited current drift. Zero matches fails the gate.
- [ ] Use `local`, `persistent`, and attached database consistently. Retain
      `workspace` only for deprecated compatibility names/resource URI/history.
- [ ] Position chart as a quick diagnostic and document all current delivery and
      presentation options. Fix the concise README's PNG-only/nonexistent-path
      example and temporal-axis comments.
- [ ] Correct read-only lists: include actual guarded tools; keep
      `unwatch_directory` and Hyper export allowed. Correct CLI help claiming
      Hyper export is disabled.
- [ ] Document doctor identity provenance and side-effect-free behavior,
      contextual `RESOURCE_BUSY`, daemon port behavior, and degraded status.
- [ ] Keep tool descriptions concise enough to remain under the Task 1 budget;
      move long operational guidance to `get_readme`/README.
- [ ] Append Added/Fixed/Changed bullets to the crate changelog only. Do not edit
      versions, root changelog, or release manifests.
- [ ] Run:

  ```bash
  cargo test -p hyperdb-mcp --test readme_tests public_docs_database_and_read_only_contract -- --exact --nocapture
  cargo test -p hyperdb-mcp --test readme_tests smoke_demo_and_changelog_contract -- --exact --nocapture
  cargo test -p hyperdb-mcp --test doctor_tests cli_help_matches_hyperd_and_read_only_contract -- --exact --nocapture
  cargo test -p hyperdb-mcp --test readme_tests
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp --test resource_tests
  cargo test -p hyperdb-mcp --test tool_schema_tests -- --nocapture
  node --check hyperdb-mcp/npm/bin.js
  git diff --check
  ```

  Expected: PASS; 33 tools and catalog budget retained.
- [ ] Review spotlight: code/doc truth, stale workspace terminology, exact tool
      guards, unsupported promises, and changelog policy.
- [ ] Commit: `fix(mcp): align agent UX and diagnostics documentation`

### Task 15: Integrated verification, adversarial review, and durable memory

**Owners:** independent validator/reviewers; main thread reconciles; writer or
doc-editor creates the final note only from verified evidence.
**Source files:** entire branch diff.
**External artifact:**
`/Users/ssteiner/dev/ssteiner-ai/notes/hyperdb-mcp-agent-ux-implementation-2026-08-14.md`

- [ ] Confirm the Hyper worktree is clean except intended changes and inspect the
      complete `origin/main...HEAD` diff and commit list.
- [ ] Run pre-review gates with captured exit codes/output:

  ```bash
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp
  HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test --workspace --exclude hyperdb-api-node --exclude hyperdb-bootstrap
  node --test hyperdb-mcp/npm/bin.test.js
  node --check hyperdb-mcp/npm/bin.js
  git diff --check origin/main...HEAD
  ```

- [ ] Dispatch two independent final reviewers in parallel:
      fast/mechanical (requirements, tests, docs, unsafe casts, dead code,
      over-engineering) and deep/architectural (cross-file contracts, daemon
      races, security/privacy, compatibility, chart correctness, incomplete
      routing). Supply the complete diff and verification evidence.
- [ ] Reconcile each finding against source. Critical/Important findings go to a
      fresh engineer, then a fresh re-review. Record rejected false positives and
      their evidence rather than silently unioning reviewer lists.
- [ ] After every accepted review fix is independently re-reviewed, the main
      thread stages only explicit source/test/doc paths, makes the appropriate
      Conventional Commit, and confirms the Hyper worktree is clean. Inspect
      `origin/main...HEAD` again so final review fixes cannot remain uncommitted
      and therefore disappear from the reviewed diff.
- [ ] After that last source commit, rerun the entire gate block above. Only this
      clean, post-fix, post-commit run can support the final green claim.
- [ ] Re-run `tool_schema_tests -- --nocapture` and record before/after total,
      descriptions, schemas, largest tools, initialization instructions, and
      `get_readme`. Call bytes bytes; any token estimate must name the actual
      client/model tokenizer or remain explicitly unavailable.
- [ ] Read the writer/doc-editor role profile before dispatch. Create the dated
      `ssteiner-ai` note using `apply_patch`; the filesystem write may require
      escalation and is limited to this exact note plus an applicable role
      profile only when the evidence supports a LEARNINGS LOG entry. Do not
      change any other `ssteiner-ai` path. The note must include:

  - Codex Desktop and model/reasoning provenance available to the session;
  - source repo/base/worktree/branch and ordered commits;
  - a self-contained ordered copy/summary of the approved implementation plan,
    plus immutable source commit/file references; do not rely on a link into the
    disposable worktree as the only plan record;
  - implemented, altered, and explicitly deferred scope;
  - before/after tool-catalog and any build/dependency measurements;
  - exact final commands, exit codes, pass/ignored counts, and environmental
    loopback note;
  - per-task and integrated reviewer identities, findings, fixes, false-positive
    reconciliations, and final verdicts;
  - operational gotchas and reusable architectural/test lessons; and
  - recommended follow-ups for future HyperDB MCP sessions.

- [ ] If the run produced a genuinely reusable lesson for engineer, tester,
      reviewer, writer, or doc-editor, update that profile's LEARNINGS LOG in
      `/Users/ssteiner/dev/ssteiner-ai/.Codex/agents/` (newest first, dated,
      source named, stale lesson superseded/pruned). Do not add generic praise or
      one-off implementation facts as role memory.
- [ ] Run `git diff --check` in `ssteiner-ai` and inspect its pre-existing dirty
      state so only requested note/profile paths are reported. Do not commit or
      push the second repository unless separately authorized.
- [ ] Dispatch a fresh read-only fact/consistency reviewer after the note is
      complete. It compares every note claim and count with immutable Hyper
      commits, the final plan/spec, captured red/green/gate evidence, and
      reviewer verdicts. Note-only corrections return to the writer/editor and
      are rechecked. If this audit uncovers a source defect, return it through a
      fresh engineer, task/integrated re-review, explicit source commit, full
      post-fix gate block, note update, and note re-review.
- [ ] After the last audited note/profile correction, rerun `git diff --check`
      in `ssteiner-ai` and repeat the requested-path allowlist/dirty-state
      inspection. This final mechanical check supersedes the earlier pre-audit
      snapshot.
- [ ] Final handoff reports Hyper branch/path/commits, exact gate evidence,
      reviewer verdicts, unresolved Minor/deferred items, and the clickable
      `ssteiner-ai` note. No PR, push, merge, release, or publication.

---

## Plan completion gate

Implementation is not complete until all of the following are true:

- Tasks 1-14 have passed their independent task-review gates;
- Task 15 has two independent integrated source-review verdicts plus a separate
  read-only fact/consistency verdict on the completed durable note;
- no final reviewer has an unresolved Critical or Important finding;
- strict workspace Clippy and both required Rust test gates have fresh captured
  zero exits after the final code change;
- npm wrapper tests/checks and tool-catalog budget are green;
- the full tool surface remains the same 33 names and local remains the default;
- public `ChartOptions` and `DaemonInfo` are unchanged;
- crate documentation/changelog match actual behavior; and
- the requested `ssteiner-ai` results/memory document exists and contains the
  plan, evidence, reviews, and reusable follow-up context.
