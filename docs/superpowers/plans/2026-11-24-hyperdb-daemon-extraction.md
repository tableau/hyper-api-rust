# `hyperdb-daemon` extraction — implementation plan

**Design:** [`../specs/2026-11-24-hyperdb-daemon-extraction-design.md`](../specs/2026-11-24-hyperdb-daemon-extraction-design.md)
**Status:** Not started. Phases 0, S and A are fully specified. Phase B is an
outline only and needs a spec revision first.

This plan is written for implementation agents, one task per dispatch, with an
independent reviewer between phases. Read the design first; decision numbers
(D1–D9) below refer to it.

## How to run this plan

- **One task per engineer dispatch.** Each task lists its files, its steps and
  its acceptance check. Do not start the next task until the current one's
  check passes with real, captured output.
- **Review at each phase boundary,** by a reviewer who did not write the code.
  The phase is done when the reviewer reports no Critical or Important findings.
- **Phase order:** 0 and S are independent and can run in parallel. A can start
  any time. B starts only after the maintainer's go decision (design, "Open
  decisions").
- **One PR per phase** from the maintainer's fork to `tableau/hyper-api-rust`.
  Commit subjects follow Conventional Commits; see per-phase guidance below.

## Rules that apply to every task

These come from `AGENTS.md` and are restated because each has caused a real
failure in this repository.

1. **Set an absolute `HYPERD_PATH`** for every cargo test run:
   `export HYPERD_PATH="$PWD/.hyperd/current"` from the workspace root. The
   daemon tests re-exec children with a relocated working directory, and an
   unset or relative value makes them fail.
2. **Never claim a pass without output.** Check exit codes. If a command prints
   nothing for about 30 seconds, report it as hanging.
3. **Never invent `hyperd` flags or parameters.** Use only what `run.rs`
   already sets, plus the ones design D6 verified against the pinned binary:
   the `config` and `strict_settings_mode` parameters and the
   `authorized_users` setting inside the config file. Nothing else.
4. **No narrowing integer `as` casts.** Use `TryFrom` (AGENTS.md reminder 7).
5. **Never hand-edit a crate version** or the root `CHANGELOG.md`. Per-crate
   changelogs are hand-maintained; append to the existing `## [Unreleased]`
   subsection rather than adding a duplicate heading.
6. **Wire compatibility is a hard requirement for Phases S and A.** The PONG
   reply, `daemon.json` keys, `STATUS` JSON, and file locations must stay
   byte-for-byte compatible except where a task says otherwise.

## The gate

Run from the workspace root. Every task ends with the subset its acceptance
check names; every phase ends with all of it.

```bash
export HYPERD_PATH="$PWD/.hyperd/current"
.hyperd/current/hyperd --version          # must match hyperdb-bootstrap/hyperd-version.toml
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --exclude hyperdb-api-node
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p hyperdb-mcp   # plus -p hyperdb-daemon once it exists
npx markdownlint-cli2
```

On macOS, also run Windows-target clippy before pushing, because `control.rs`
and `state_perms.rs` have Windows-only code that only CI otherwise compiles:

```bash
cargo clippy --target x86_64-pc-windows-msvc -p hyperdb-mcp --all-targets --keep-going -- -D warnings
```

This needs the stubbed `cc`/`ar`/`nasm` setup for `ring`; if it is not
available, say so in the report rather than skipping silently.

---

## Phase 0 — Measure

**Goal:** the numbers that decide whether Phase B is worth building.

### Task 0.1 — `hyperd` startup and idle-memory measurement

- **Add** `hyperdb-api/benches/hyperd_startup_cost.rs`, beside
  `benchmark_suite.rs`, and register it in `hyperdb-api/Cargo.toml` the same
  way `benchmark_suite` is registered, so it runs with `--example`. It runs N iterations
  (default 30, from argv) of: `HyperProcess::new(None, None)`, connect, run
  `SELECT 1`, drop. Report per-phase wall time (spawn-to-endpoint, connect,
  first query, shutdown) as min, median and p95. Separately, start one
  `hyperd`, wait 5 seconds idle, and report its resident memory: `ps -o rss=`
  on Unix, and on Windows the working set via `tasklist` or skip with a note.
  Also report the connect time against an engine that requires SCRAM (design
  D6: build the config file as in Task S.4, or reuse its helper if S.4 has
  landed) next to the password-less connect time, so the per-connection SCRAM
  cost is known.
- **Run** it in release mode for both transports:

  ```bash
  cargo run -p hyperdb-api --release --example hyperd_startup_cost -- 30
  BENCH_TRANSPORT=ipc cargo run -p hyperdb-api --release --example hyperd_startup_cost -- 30
  ```

  `BENCH_TRANSPORT` is the existing switch parsed in
  `hyperdb-api/benches/benchmark_suite.rs`; reuse its parsing rather than
  adding a new variable.
- **Record** the results, host details and `hyperd --version` in a new section
  of `docs/BENCHMARK_GUIDE.md`.
- **Acceptance:** the example compiles under the gate's clippy, the numbers are
  pasted into the PR with the raw output, and markdownlint passes.
- **Commit:** `docs(bench): measure hyperd startup cost and idle memory`.

---

## Phase S — Engine and control-channel hardening (no extraction needed)

**Goal:** D5, D6 and D7. Ships in `hyperdb-mcp` (and, for S.3, in
`hyperdb-api-core` and `hyperdb-api`) today, and moves with the code in Phase A.

**Order:** S.1 and S.3 are independent. S.4 needs S.3. S.2 goes last, because
it documents what S.4 built. Phase S can ship as two PRs: S.1 alone, then
S.3 + S.4 + S.2.

### Task S.1 — Capability token for `STOP` and `REPORT_HYPERD_ERROR`

- **Files:** `hyperdb-mcp/src/daemon/discovery.rs` (record field),
  `health.rs` (command dispatch and client helpers), `run.rs` (token
  generation at start), `spawn.rs` (takeover sends `STOP`), and callers in
  `engine.rs`, `server.rs`, `main.rs`.
- **Steps:**
  1. Generate 32 random bytes per daemon start from the OS RNG. Check
     `Cargo.lock` for a crate already in the tree (for example `getrandom`)
     before adding a dependency, and prefer an existing one. Hex-encode it.
  2. Add an optional `control_token` field to `DaemonRecord`, written to
     `daemon.json`. The file is already owner-only; keep it that way, and add
     the field with `#[serde(default)]` so older records still parse.
  3. The command grammar becomes `STOP <token>` and
     `REPORT_HYPERD_ERROR <token>`. `PING`, `HEARTBEAT` and `STATUS` stay
     unauthenticated. Compare tokens in constant time.
  4. A wrong or missing token gets a distinct error reply (for example
     `ERR unauthorized`) and a `warn`-level log line that does **not** include
     either token.
  5. Update every client call site to read the token from the discovery record
     it already holds and send it.
  6. **Compatibility.** A newer client meeting an older daemon (record has no
     `control_token`) sends the bare command, as today. An older client
     meeting a newer daemon gets `ERR unauthorized` on `STOP`; the MCP's
     takeover path then treats it like any failed takeover. Document this in
     the `hyperdb-mcp` changelog.
  7. Never print the token in `STATUS`, the doctor output or any log.
- **Tests** (unit tests in the touched modules, plus `hyperdb-mcp/tests/daemon_tests.rs`):
  `STOP` with the correct token stops the daemon; `STOP` with no token or a
  wrong token is rejected and the daemon keeps running; same for
  `REPORT_HYPERD_ERROR` (three wrong-token reports must **not** trip
  `RESTART_LIMIT`); a record without the field parses; `STATUS` and the doctor
  output contain no token.
- **Acceptance:** the full gate.
- **Commit:** `fix(mcp): require a per-daemon token for STOP and REPORT_HYPERD_ERROR`.

### Task S.3 — Strict SCRAM mode in the client

- **Files:** `hyperdb-api-core/src/client/config.rs` (new flag),
  `connection.rs` and `async_connection.rs` (the authentication loops, around
  the `Message::Authentication*` arms), and the builders in
  `hyperdb-api/src/connection_builder.rs` and `async_connection_builder.rs`.
  Check whether the pool's config (`hyperdb-api/src/pool*`) builds its own
  connection config; if so, carry the flag through it too.
- **Steps:**
  1. Add a strict flag to the core `Config`, off by default. Provisional
     builder method name: `require_scram_sha256(bool)` on both builders.
     Choose the final name to match the builders' existing style, and give it
     a `///` doc comment that explains what it rejects.
  2. When the flag is on, the authentication loop must:
     - reject `AuthenticationCleartextPassword` and `AuthenticationMd5Password`
       with an authentication error, **without** sending anything;
     - reject `AuthenticationOk` unless a SASL final message has already been
       verified by `scram_verify_server` in this connection;
     - reject a SASL mechanism list without `SCRAM-SHA-256` (already the case);
     - fail if the flag is on and no password was configured, before
       connecting.
  3. Behaviour with the flag off is byte-for-byte unchanged.
  4. The error messages name the method the server asked for, and never
     include the password.
- **Tests:** a tiny in-test fake server (a `std::net::TcpListener` on
  `127.0.0.1:0` that reads the startup message and replies with one canned
  authentication message) is enough. For each of `AuthenticationOk`,
  cleartext and MD5, assert that strict mode fails, and that the fake server
  received no password message. For cleartext, assert the password bytes never
  arrive. Also test the happy path against a real `hyperd` started as in
  Task S.4, sync and async. With the flag off, the existing auth tests still
  pass unchanged.
- **Changelogs:** `### Added` bullets in `hyperdb-api/CHANGELOG.md` (the builder
  methods) and `hyperdb-api-core/CHANGELOG.md`.
- **Acceptance:** the full gate.
- **Commit:** `feat(api): add a strict SCRAM-SHA-256 mode to the connection builders`.

### Task S.4 — Shared engine on TCP with a generated password

Design D5 and D6 hold the rationale and the probe results; read them first.

- **Files:** `hyperdb-mcp/src/daemon/run.rs` (`build_params`, startup, the
  supervisor's restart path), a new `hyperdb-mcp/src/daemon/engine_auth.rs`
  (password, verifier, config file), `discovery.rs` (record field),
  `state_perms.rs` (owner-only file creation, if a helper is missing), and the
  code that connects to the engine: `hyperdb-mcp/src/engine.rs` and any
  connection the daemon makes itself (health checks in `run.rs` or
  `health.rs`).
- **Steps:**
  1. **Password and verifier.** Once per daemon start, generate 32 random
     bytes from the OS RNG (reuse whatever S.1 chose) and encode them as
     URL-safe text. Compute the PostgreSQL SCRAM-SHA-256 verifier
     `SCRAM-SHA-256$4096:<b64 salt>$<b64 StoredKey>:<b64 ServerKey>` with a
     16-byte random salt, reusing the PBKDF2 and HMAC dependencies
     `hyperdb-api-core/src/client/auth.rs` already uses (`pbkdf2`, `hmac`,
     `sha2`). Unit-test the verifier against a fixed salt and password, and
     test that a real `hyperd` accepts it (see Tests).
  2. **Config file.** Write `<state-dir>/engine/hyperd-auth.json` with exactly
     the JSON in design D6. Create it owner-only at creation time (Unix:
     `OpenOptions::mode(0o600)` plus `create_new` after removing a stale
     file; Windows: the existing owner-only helpers in `state_perms.rs`).
     Write it before starting `hyperd`.
  3. **Engine parameters.** In `build_params`, replace
     `TransportMode::Ipc` with `TransportMode::Tcp`, drop the
     `domain_socket_directory` setup, set the listen mode **explicitly** to
     PostgreSQL-protocol only (the `ListenMode` variant for libpq; do not rely
     on `HyperProcess`'s default), and set `config` to the file path and
     `strict_settings_mode` to `""` (it is a valueless flag; `HyperProcess`
     emits `--strict-settings-mode` for an empty value). Leave the default
     `--no-password` alone: `authorized_users` overrides it, and strict mode
     plus step 4 cover the case where it does not. Strict mode applies to
     **every** setting the daemon passes (`log_file_max_count`,
     `log_file_size_limit` and the rest), not only the config file. The probe
     only covered `HyperProcess`'s defaults plus `log_dir`, so confirm the
     daemon's full parameter set starts under strict mode before relying on it.
  4. **Self-check.** After `hyperd` reports its endpoint and before
     `daemon.json` is written, connect with **no** password. The connection
     must fail with an authentication error. If it succeeds, stop `hyperd`,
     log an `error`, and fail startup. Then connect with the password and
     strict SCRAM on, and run `SELECT 1`.
  5. **Discovery record.** Add `engine_password` to `DaemonRecord` with
     `#[serde(default)]`. Never print it in `STATUS`, the doctor, or any log.
     The password stays the same for the daemon's lifetime, including
     supervisor restarts of `hyperd`, so clients keep working across a
     restart; the config file is reused as-is on restart.
  6. **Clients.** Every connection to the shared engine uses the record's
     password with strict SCRAM on. A record with a TCP engine endpoint but no
     `engine_password` is treated as unusable, never as "connect without a
     password". A record from an older daemon (IPC endpoint, no password)
     keeps working as today.
  7. **Mixed versions.** An older client meeting a newer daemon fails to
     authenticate. Make sure that surfaces as a clear error (and, where the
     MCP has a takeover path, that it behaves like any failed attach). Note it
     in the `hyperdb-mcp` changelog.
     Per design D7a, pre-1.0 clients are unsupported against a 1.0 daemon:
     the daemon reports a rejected client in `STATUS` and its log, naming the
     upgrade, and a test records what an old client sees so the changelog can
     quote it.
  8. **Fallback, documented only.** In `engine_auth.rs`'s module docs, record
     the environment-variable route from design D6 and what switching to it
     takes. Do not implement it.
- **Tests** (`hyperdb-mcp/tests/daemon_tests.rs` and unit tests):
  - The daemon's engine endpoint is TCP on `127.0.0.1`, and the engine has no
    gRPC listener (`HyperProcess::grpc_endpoint()` is `None`).
  - A unit test on `build_params` asserts the transport is TCP and the listen
    mode is PostgreSQL-protocol only, so a later edit cannot quietly add IPC
    or gRPC.
  - A connection without a password and one with a wrong password are both
    rejected; the record's password connects.
  - **Pin-bump guard:** starting `hyperd` with a config file containing a
    misspelt setting name (for example `authorized_userz`) and strict mode
    fails startup. Name the test so the `update-hyperd-release` run makes it
    obvious, for example `hyperd_auth_config_strict_mode_guard`.
  - The self-check fails startup when the engine accepts a password-less
    connection. Inject that through a test seam (for example a config file
    with `"method": "trusted"`), not by changing production code paths.
  - The config file and `daemon.json` are owner-only (Unix mode check; on
    Windows, reuse whatever the existing permission tests check).
  - The password is absent from `STATUS`, the doctor output, the daemon log,
    and (Unix) `ps eww -p <hyperd pid>`.
  - The password survives a supervisor restart of `hyperd`.
- **Also:** add a line to `.claude/skills/update-hyperd-release/SKILL.md`
  saying that a failure of the auth-config guard test means `authorized_users`
  changed, and pointing to design D6's fallback.
- **Acceptance:** the full gate, including Windows-target clippy.
- **Commit:** `fix(mcp): run the shared engine on TCP with a generated SCRAM password`.

### Task S.2 — Document the engine trust model

- **Files:** module docs in `hyperdb-mcp/src/daemon/mod.rs`, `run.rs`
  (`build_params`) and `engine_auth.rs`, and `hyperdb-mcp/README.md` if it
  describes the daemon.
- **Content:** the shared engine listens on loopback TCP; connecting requires
  a per-daemon password whose plaintext lives only in the owner-only
  `daemon.json`; `hyperd` holds only its SCRAM verifier, in an owner-only
  config file; clients require SCRAM-SHA-256 with server verification; anyone
  running as the same user can read the password and so reach the engine;
  the daemon refuses to publish an engine that accepts password-less
  connections. State these as properties, not as a weakness walkthrough.
- **Acceptance:** `cargo doc` for `hyperdb-mcp` with `-D warnings`, and
  markdownlint.
- **Commit:** `docs(mcp): document the shared engine's trust model`.

---

## Phase A — Move the daemon into `hyperdb-daemon`

**Goal:** D1–D4. Behaviour, wire formats and the MCP tool surface are
unchanged. Tasks A.1 and A.2 refactor in place so that A.3 is a pure move.

### Task A.1 — Product-identity seam (in place)

- **Files:** `hyperdb-mcp/src/daemon/{spawn,health,discovery,run}.rs`,
  `hyperdb-mcp/src/main.rs`, `hyperdb-mcp/src/engine.rs`.
- **Steps:** add `DaemonIdentity` (design D3) to `daemon/mod.rs`. Thread it
  through `DaemonConfig` and `ensure_daemon` (and anything else that reads
  `crate::version`). The four `crate::version::…` references in `daemon/` must
  disappear; only `main.rs` and `engine.rs` construct the identity, from
  `crate::version`. Keep the PONG token, the `mcp_version` JSON key and the
  takeover semantics exactly as they are.
- **Acceptance:** `rg -n 'crate::version' hyperdb-mcp/src/daemon` prints
  nothing; full gate passes.
- **Commit:** `refactor(mcp): pass the daemon's product identity in rather than reading MCP_VERSION`.

### Task A.2 — Move `ReportedPath` and `PathEncoding` (in place)

- **Steps:** move both types from `diagnostics.rs` into a new
  `hyperdb-mcp/src/daemon/report_path.rs`, and `pub use` them from
  `diagnostics.rs` so every existing path still resolves. Update the doc
  comment in `discovery.rs` that links `crate::paths::persistent_home_dir` so
  it no longer depends on `paths` (plain text, or drop the link).
- **Acceptance:** `rg -n 'crate::(diagnostics|paths|version)' hyperdb-mcp/src/daemon`
  prints nothing; full gate passes.
- **Commit:** `refactor(mcp): make the daemon module self-contained`.

### Task A.3 — Create the crate and move the modules

- **Steps:**
  1. `git mv hyperdb-mcp/src/daemon hyperdb-daemon/src`, then rename
     `hyperdb-daemon/src/mod.rs` to `lib.rs`. Use `git mv` so history follows.
  2. Write `hyperdb-daemon/Cargo.toml` modelled on `hyperdb-bootstrap/Cargo.toml`
     and `hyperdb-mcp/Cargo.toml`: `version.workspace = true`, the workspace
     `edition`, `license`, `repository` and `[lints] workspace = true`. Copy
     only the dependencies the moved code uses (design, finding 1), with the
     same feature sets and the same `cfg(unix)` / `cfg(windows)` split. Pin
     `hyperdb-api` exactly as `hyperdb-mcp` does, inside its own
     `# x-release-please-start-version` / `# x-release-please-end` markers.
  3. Inside the moved files, replace `crate::daemon::` with `crate::`.
  4. Add `"hyperdb-daemon"` to the root `Cargo.toml` `members`.
  5. In `hyperdb-mcp/Cargo.toml`, add
     `hyperdb-daemon = { path = "../hyperdb-daemon", version = "=<current workspace version>" }`
     **inside** the existing release-please marker block next to
     `hyperdb-api`, so the pin is bumped in lockstep. Remove any dependency
     `hyperdb-mcp` no longer uses; CI builds with `CARGO_BUILD_WARNINGS`, so
     cargo's unused-dependency warnings fail the build.
  6. In `hyperdb-mcp/src/lib.rs`, replace `pub mod daemon;` with
     `pub use hyperdb_daemon as daemon;` keeping its `#[doc(hidden)]`, and
     update the architecture bullet.
  7. Fix visibility: anything `pub(crate)` that `hyperdb-mcp` still uses must
     become `pub`. Let the compiler list them; do not widen anything else.
- **Acceptance:** `cargo build --workspace` and `cargo clippy` pass. Tests are
  A.4.
- **Commit:** `refactor(daemon): move the shared-hyperd daemon into a hyperdb-daemon crate`.

### Task A.4 — Fix test re-exec paths and prove the tests still run

The moved unit tests in `discovery.rs` (and possibly others) re-exec their own
test binary with a filter string naming the test, for example
`"daemon::discovery::tests::raw_discovery_rejects_fifo_without_blocking"`.
After the move the binary is `hyperdb-daemon`'s and the path is
`discovery::tests::…`.

- **Steps:**
  1. `rg -n '"daemon::' hyperdb-daemon/src` and update every hit.
  2. For each re-exec helper, make the parent assert the child actually ran
     one test (for example by checking the child's stdout for
     `1 passed` or `running 1 test`), so a stale filter fails loudly instead
     of passing vacuously. If such an assertion already exists, confirm it.
  3. Run both crates' tests and compare counts with the pre-move baseline:
     `cargo test -p hyperdb-mcp` on `upstream/main` before A.1, versus
     `cargo test -p hyperdb-daemon` plus `cargo test -p hyperdb-mcp` now. The
     totals must match; list any difference with a reason.
- **Acceptance:** full gate; the test-count comparison pasted into the report.
- **Commit:** fold into A.3's commit or a follow-up `test(daemon): …`.

### Task A.5 — Release and CI plumbing

A missing entry here fails the *release*, not CI, so check every site.
Discovery method: `rg -n 'hyperdb-bootstrap' .github Makefile build.ps1 *.md docs release-please-config.json`
lists the places that enumerate crates.

- **Must change:**
  - `.github/workflows/release.yml`: add `publish hyperdb-daemon` **after**
    `hyperdb-api` and **before** `hyperdb-mcp`, and update the dependency-edge
    comment block above the publish calls (`hyperdb-daemon → hyperdb-api`,
    `hyperdb-mcp → hyperdb-daemon`).
  - `release-please-config.json`: add `hyperdb-daemon/Cargo.toml` to
    `extra-files` as a `generic` entry, like the other crates with markers.
  - `.github/workflows/ci.yml`: add `-p hyperdb-daemon` to the `doc` job, and
    to any publish-dry-run or per-crate list.
  - `Makefile` `doc` target and its "Covers all 8 workspace members" comment;
    `build.ps1` `$DocCrates` (and `$Crates` if the daemon should build with
    the MCP).
- **Check, and update comments if they enumerate crates:**
  `.github/workflows/release-please.yml` (the lockfile sentinel derives members
  from `cargo metadata`, so only its comments change),
  `.github/dependabot.yml` (root `/` already covers it), `deny.toml` and
  `about.toml` (no new licenses expected).
- **Acceptance:** `cargo publish --dry-run -p hyperdb-daemon` succeeds (it can,
  because the pinned `hyperdb-api` version is already on crates.io), and the
  gate's `cargo doc` with both crates passes.
- **Commit:** `ci(release): publish hyperdb-daemon before hyperdb-mcp`.

### Task A.6 — Documentation

- `hyperdb-daemon/README.md`: what it is, and a prominent "not a public API
  yet" statement modelled on `hyperdb-api-core/README.md`. Link the design.
- `hyperdb-daemon/CHANGELOG.md`: a new file with one `## [Unreleased]` section
  and an `### Added` bullet. Copy the header format from a sibling crate.
- `hyperdb-daemon/src/lib.rs`: crate docs with the same internal positioning,
  the trust-model text from S.2, and the module map.
- `AGENTS.md`: add the crate to the architecture layer list ("`hyperd` daemon →
  `hyperdb-daemon`"), and change the per-crate changelog count from nine to
  ten in reminder 8, adding `hyperdb-daemon` to its list. Check
  `CONTRIBUTING.md` and `docs/GITHUB_OPERATIONS.md` for crate lists or publish
  order and update them.
- `hyperdb-mcp/CHANGELOG.md`: nothing required (the library target is not
  public), unless S.1 has not shipped yet.
- **Acceptance:** `npx markdownlint-cli2` with no new findings versus
  `upstream/main`; `cargo doc` for `hyperdb-daemon` with `-D warnings`.
- **Commit:** `docs(daemon): document the hyperdb-daemon crate`.

### Phase A exit

- Full gate, including Windows-target clippy.
- **End-to-end smoke**, with a scratch `HYPERDB_STATE_DIR` so the
  maintainer's real daemon is untouched: build `hyperdb-mcp` in release; start
  two MCP clients against it (the end-to-end tests show how to drive one over
  stdio); confirm `hyperdb-mcp daemon status` reports one daemon with the same
  PID for both; confirm `hyperdb-mcp doctor` is clean; then
  `hyperdb-mcp daemon stop` and confirm the lock is released.
- Independent review. Then one PR. Suggested PR title:
  `refactor(daemon): extract the shared-hyperd daemon into hyperdb-daemon`.
  Note `refactor:` does not cut a release on its own; it ships with the next
  `fix:` or `feat:`.

---

## Phase B — Public `acquire()` API (outline; spec revision required)

Prerequisites: the maintainer's go decision from Phase 0's numbers, decision
D8 made, Phase S shipped, and issue #242 (recovery from an unresponsive
`hyperd`) fixed.

Expected tasks, to be specified in a design revision:

1. **Protocol version.** Integer in `daemon.json` and the `PING` reply; clients
   accept N and N-1 and treat anything else as "no daemon".
2. **Cohort state directories.** `~/.hyperdb/daemons/<cohort-hash>/` for
   library daemons; the MCP keeps `~/.hyperdb`.
3. **Leases.** `ACQUIRE` / `RELEASE` commands, lease count in `STATUS`, idle
   timer runs only at zero leases, heartbeats expire dead clients' leases.
4. **Launcher** per D8, likely a `hyperdb-daemon` binary target plus a lookup
   order (`HYPERDB_DAEMON_PATH`, beside `hyperd`, `PATH`), and its distribution
   story.
5. **API surface.** `Acquisition`, `Options` (`#[non_exhaustive]`), `Engine`,
   `SharedEngine`, `acquire` and `acquire_async`, typed errors, and the
   `SharedOrPrivate` fallback deadline.
6. **Library defaults.** Idle timeout on by default; libraries never take over
   an older daemon.
7. **Docs and examples.** A shared-engine example under `hyperdb-api/examples/`
   or the new crate, the IPC-versus-TCP performance trade-off (design D5), and
   the trust model.
8. **Security review** of the protocol changes before release.

Phase B makes `hyperdb-daemon` a public API, so it is a `feat:` and needs a
`### Added` entry in its changelog.
