# Post-`1.0.0-rc.1` Cleanup Implementation Plan

Closes the findings left open when `1.0.0-rc.1` shipped. Nothing here blocks
the RC; the point is to land it before promoting to `1.0.0`, so the final
release carries no known defects.

Sources: the three adversarial reviews run against PR #250 (data-path,
edition-2024 mechanics, CI/release claims), plus items discovered while cutting
the release. Every item below was re-verified against `main` at `033b2da` on
2026-09-05 — none are stale.

**Scope decision: non-Node work first.** Tasks 1–9 touch Rust core, CI, and
docs. The `hyperdb-api-node` findings are deliberately deferred to Tasks 10–13
so the N-API surface changes land as one reviewable group rather than being
interleaved with unrelated work.

## Global constraints

- **Branch:** `chore/post-rc-cleanup`, already open with the rust-analyzer
  registry fix (`22b0a7a`). Base is `upstream/main`, not `origin/main` — the
  fork's `main` lags.
- **Reminder 7 applies.** Any narrowing integer `as` cast encountered must
  become a `TryFrom`, even incidentally.
- **Reminder 8 applies.** Public-API changes need a per-crate `CHANGELOG.md`
  bullet under `## [Unreleased]`. Internal refactors do not.
- **Reminder 10 applies.** No task is complete without captured command output
  and a checked exit code.
- **`hyperdb-compile-check` is outside the workspace.** `cargo clippy
  --workspace` and `cargo test --workspace` skip it. Verify it explicitly with
  `--manifest-path hyperdb-compile-check/Cargo.toml`.
- **`make test` covers only 3 of 8 crates** (`hyperdb-api-core`, `hyperdb-api`,
  `hyperdb-mcp`). Use `cargo test --workspace` to match CI, which yields 1568
  rather than 1519.
- Do not touch `.agents/` or `.codex/` — untracked local config, deliberately
  left alone.

## Verification gate

Every task ends with the subset relevant to it; the plan-completion gate runs
all of it:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy --manifest-path hyperdb-compile-check/Cargo.toml --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p hyperdb-api -p hyperdb-api-core \
  -p hyperdb-api-derive -p hyperdb-api-node -p hyperdb-api-salesforce \
  -p hyperdb-bootstrap -p hyperdb-mcp -p sea-query-hyperdb
cargo deny check
cargo audit --deny warnings
cargo test --workspace
cargo test --manifest-path hyperdb-compile-check/Cargo.toml
cargo +1.88 check --workspace --locked --all-targets --all-features
cargo +1.88 check --locked --all-targets --manifest-path hyperdb-compile-check/Cargo.toml
```

## File map

| Area | Files |
|---|---|
| Discarded Arrow errors | `hyperdb-api-core/src/client/grpc/authenticated_client.rs` |
| Dead crate name | `hyperdb-api/src/process.rs`, `hyperdb-bootstrap/src/bin/hyperdb-bootstrap.rs`, `hyperdb-bootstrap/src/lib.rs`, `hyperdb-bootstrap/src/error.rs`, `hyperdb-bootstrap/src/release.rs`, `hyperdb-bootstrap/tests/integration.rs` |
| Windows-gated lint | `hyperdb-api/src/process.rs` |
| Lint drift | `hyperdb-compile-check/Cargo.toml` |
| Stale comment | `hyperdb-api-core/src/protocol/types.rs` |
| CI hardening | `.github/workflows/rhel-compatibility.yml`, `.github/workflows/ci.yml`, `Makefile` |
| Docs | `AGENTS.md`, `docs/BENCHMARK_GUIDE.md`, `CONTRIBUTING.md` |
| Node (deferred) | `hyperdb-api-node/index.d.ts`, `hyperdb-api-node/src/{inserter,columnar,result}.rs` |

---

## Task 1: Stop silently discarding Arrow decode errors

**Highest correctness weight in the plan.** `authenticated_client.rs:976` and
`:1064` both do `if let Ok(batch) = batch_result`, dropping per-batch decode
failures on the floor. A corrupt batch mid-stream therefore yields a *silently
partial* label map from `get_table_labels` / `get_column_labels` — the caller
cannot distinguish "this table has no labels" from "decoding failed halfway".

The `#[expect(clippy::manual_flatten)]` waivers that previously documented this
as deliberate were removed during the edition-2024 let-chain sweep, which took
the recorded intent with them. That is why this looks like an oversight now.

**Decide first, then implement.** Two defensible outcomes:

1. **Propagate** — return the decode error. Correct, but changes behavior for
   any caller currently tolerating partial results, so it needs a
   `hyperdb-api-core` changelog bullet and a look at both call sites' callers.
2. **Keep discarding, but record why** — restore the intent in a comment and
   log at `warn` so a partial map is at least observable.

Option 1 is preferred unless a caller depends on partial success. Establish
that by reading the callers of both functions before choosing.

**Verify:** `cargo test --workspace`; add a unit test that feeds a truncated
Arrow IPC stream and asserts the chosen behavior (error propagated, or warning
emitted and result marked partial).

## Task 2: Fix the dead `hyperd-bootstrap` command in user-facing errors

`hyperdb-api/src/process.rs:286` instructs users to run:

```text
cargo run -p hyperd-bootstrap -- download
```

That package has not existed since the rename to `hyperdb-bootstrap`; the
command fails with *"package ID specification `hyperd-bootstrap` did not match
any packages"*. This is the **first thing a new user sees** when `HYPERD_PATH`
is unset, so it is the highest annoyance-per-minute item here.

Also stale, in descending user visibility:

- `hyperdb-bootstrap/src/bin/hyperdb-bootstrap.rs:169` — runtime error text
  `run \`hyperd-bootstrap download\` first`
- `hyperdb-api/src/process.rs:259`, `:269` — doc comments
- `hyperdb-bootstrap/src/lib.rs:9`, `error.rs:4`, `release.rs:23`,
  `tests/integration.rs:4` — doc comments

Fix the two runtime strings first; the doc comments are cosmetic but should go
in the same pass so the name is consistent.

**Verify:** `grep -rn 'hyperd-bootstrap' --include='*.rs' .` returns nothing
outside `hyperd-version.toml` paths (where `hyperdb-bootstrap/hyperd-version.toml`
is a legitimate path, not the crate name). Then confirm the suggested command
actually runs: `cargo run -p hyperdb-bootstrap --bin hyperdb-bootstrap -- --help`.

## Task 3: Fix the `collapsible_if` the let-chain sweep could not see

`hyperdb-api/src/process.rs:667-671`:

```rust
if pipe_name.is_some() {
    if let Some(ref pname) = pipe_name {
        return ConnectionEndpoint::named_pipe(".", pname);
    }
}
```

This is the exact shape the edition-2024 sweep flattened 127 of, but it sits
inside `#[cfg(windows)]`. CI's clippy job runs `ubuntu-latest` only, and
`cfg`-stripped code is removed before lints run, so it was never linted. The
claim "all 127 fixed" is therefore Linux-scoped.

The `is_some()` guard is dead weight — reduce to `if let Some(ref pname) =
pipe_name`. This is the only surviving instance found across roughly 75
platform-gated regions.

Blast radius is currently limited because neither `Makefile` nor `build.ps1`
exposes a clippy target, so a Windows contributor only hits it by running
clippy by hand. Consider adding `windows-latest` to the clippy matrix —
`ci.yml`'s own comment already anticipates this ("If a lint ever diverges by
target (rare), broaden the matrix").

**Verify:** `cargo clippy --target x86_64-pc-windows-msvc` if a Windows target
is installable locally; otherwise rely on the broadened CI matrix.

## Task 4: Close the `hyperdb-compile-check` lint drift

Root `Cargo.toml:147-148` promotes `missing_errors_doc` and
`missing_panics_doc` to `deny`. `hyperdb-compile-check/Cargo.toml:75-76` still
says `warn`. That crate declares its own `[workspace]`, so it cannot inherit
the workspace lint table and instead duplicates it verbatim — and the commit
that promoted the levels updated only the root copy.

It is also excluded from `cargo clippy --workspace`, so the "measured zero
violations" claim behind the promotion never covered it.

Mirror the two `deny` levels, then confirm the crate actually satisfies them.
If it does not, that is a finding rather than a reason to skip: fix the missing
sections.

**Verify:** `cargo clippy --manifest-path hyperdb-compile-check/Cargo.toml
--all-targets -- -D warnings`.

## Task 5: Retarget the stale `split_at_checked` comment

`hyperdb-api-core/src/protocol/types.rs:358` says *"`split_at_checked` plus
`slice::get` cannot overflow"*, but the shipped implementation uses
`split_first_chunk::<4>()` (lines 304 and 340). `split_at_checked` was the
earlier attempt that commit `d7157c3` describes discarding. Retarget the
comment so the next reader is not sent looking for a call that is not there.

**Verify:** `cargo doc` clean; grep confirms no remaining `split_at_checked`
reference.

## Task 6: Harden the RHEL workflow

Three separate issues in `.github/workflows/rhel-compatibility.yml`:

1. **`protoc` is fetched with no integrity check.** It is downloaded over
   HTTPS and unzipped into `/usr/local` as root. Version-pinning is already
   correct (`PROTOC_VERSION: '35.1'`, matching `Makefile:169`), so drift is not
   the risk — a retagged or compromised release is. Add a `sha256sum -c`
   against a pinned digest, or `gh attestation verify`.
2. **No `concurrency` group** (verified: zero `concurrency` keys). Every other
   workflow in the repo has one. Successive pushes to a PR stack multi-minute
   container jobs. Mirror `ci.yml`: `group: ${{ github.workflow }}-${{ github.ref }}`
   with `cancel-in-progress` on `pull_request`.
3. **The gate skips `hyperdb-compile-check`.** Its `cargo check --workspace`
   cannot see that crate, yet `release.yml` publishes it — so the one gate that
   proves "builds on Red Hat's toolchain with no rustup" never covers a crate
   enterprise consumers can depend on. The new `msrv` job already checks it
   separately; do the same here.

Also fold in the trivial `Makefile` fix: `help` is missing from `.PHONY`
(verified), which is pre-existing.

**Verify:** `make check-rhel` locally; then confirm the workflow is green on
the PR. Note the job is path-filtered, so a docs-only commit will not run it —
touch a `.rs` or `Cargo.toml` file to exercise it.

## Task 7: Correct the stale `AGENTS.md` Editor Setup section

`AGENTS.md:148-166` is now wrong on both of its points:

- Line 150 frames edition 2024 as something *"a few of our transitive deps
  (`rmcp`, `rmcp-macros`, `base64ct`, `clap_lex`)"* use. The entire workspace
  is edition 2024 as of `1.0.0-rc.1`.
- It instructs contributors to run `rustup component add rust-analyzer` by
  hand, which `rust-toolchain.toml` now does automatically via its `components`
  entry. Commit `091327c` claimed to obviate that instruction; the batched docs
  pass did not follow through.

Keep the `rust-analyzer.server.path` guidance — that part is still useful and
deliberately not committed to workspace settings.

**Verify:** read-through; no automated gate covers this.

## Task 8: Make the benchmark comparison traceable

`docs/BENCHMARK_GUIDE.md`'s "Rust vs Node.js — 10M apples-to-apples" table
cites Rust-at-10M figures, but that table is not in the document (verified:
zero "Rust suite — 10M" sections), so not one number in the comparison can be
checked against a source. The 100M table sits directly above it, which invites
the reader to assume — wrongly — that the comparison came from there.

Two fixes:

1. Add the Rust 10M table, or state the exact invocation that produced the
   column. The data exists: `bench_ab/final/rust10m-r*.json`, five runs, median
   taken.
2. `:232` still reads `~1 K/s` for the aggregation row. Replace with the
   measured figure.

While there, re-check the callout added in the macOS refresh: it warns that the
`× 4` rows carry ±20–61% spread and that single-connection rows are the ones to
compare, yet the very next takeaway leads with a `× 4` number and derives a
two-significant-figure "2.4× speedup" from it.

**Verify:** every figure in the comparison table appears in a table in the same
document, or has its invocation stated.

## Task 9: Reconcile `make test` with CI, and give `hyperdb-compile-check` a changelog

Two loose ends that mislead rather than break:

- **`make test` covers 3 of 8 crates**, so the "1519 passed" figure quoted
  throughout the 1.88 uplift is a subset; CI's `cargo test --workspace` gives
  1568. Either broaden the target to match CI, or rename it and document what
  it covers, so a contributor running it locally is not misled about coverage.
- **`hyperdb-compile-check` has no `CHANGELOG.md`** despite being published to
  crates.io by `release.yml:264`. It is absent from AGENTS.md reminder 8's
  eight-crate list, which is why it was missed. Either add one and extend the
  list to nine, or state explicitly why it is exempt.

**Verify:** `cargo test --workspace` count matches whatever the Makefile target
now claims.

---

## Deferred: `hyperdb-api-node` (Tasks 10–13)

Grouped so the N-API surface lands as one reviewable change. Ordered by weight.

### Task 10: Guard the `expect()` at the N-API boundary

`columnar.rs:107` is sound today — the bounds scan directly above makes the
`expect()` unreachable, verified by inspection: `LO..=HI` is inclusive, both
bounds are widening `i32 → i64` casts, and both passes iterate the same `v`
binding under one `&self` borrow with no interior mutability.

The reason to act anyway: **napi 3.10 does not wrap `#[napi]` bodies in
`catch_unwind`** — no such call exists in its `src/`. A panic here unwinds into
V8's C++ frames, which is undefined behavior, not a JS exception. The
correctness rests entirely on the two passes staying coupled, and nothing
enforces that against a future edit.

Add a `debug_assert!` in the narrowing pass, or move scan and map into one
function commented as a unit. Also add the missing `# Panics` section —
the workspace denies `missing_panics_doc` and this function has none (verified),
which pulls against commit `d7157c3`, where `protocol/types.rs` was
restructured specifically to avoid an `expect()` for that lint.

### Task 11: Name the row and column in insert rejection errors

A caller who buffers a million rows and calls `execute()` currently gets:

```text
row encoding error: value 70000 does not fit the destination SMALLINT column (valid range -32768..=32767)
```

The whole batch fails with no way to locate the offending datum. This blunts
the Task 2.6 fix: the old behavior silently wrote a wrong number, and the new
behavior says a wrong number exists *somewhere* in your data.

`encode_rows` already has both coordinates — `col_idx` from the `enumerate()`
at `inserter.rs:300`, and the row index one `.enumerate()` away on the `for row
in rows` loop at `:299`. Thread them into the `map_err` at `:305`.

### Task 12: Add the missing `@throws` to `index.d.ts` write paths

Both *read* getters were updated (`getInt32` at `:616`, `getInt32Column` at
`:230`). The three *write* paths were not — and by the change's own framing
those are the more serious half, since that is where corruption reached the
`.hyper` file. `RowInserter.addRow`, `addRows`, and `execute` carry no
`@throws`, so a TypeScript consumer gets no signal that a previously-succeeding
insert can now reject.

Put it on `execute()` — that is where the error actually surfaces, not
`addRow()`. `addColumnar` has the same gap: a value routed through the
`int64Columns` bucket into an `INT` column now goes through `narrow_i32`
(`inserter.rs:392`) and throws, which its doc block at `:433` does not mention.

### Task 13: Narrow the `cast_precision_loss` allow to its stated reason

`columnar.rs:4-7` allows `clippy::cast_precision_loss` crate-wide with the
reason *"diagnostic metric output; bounded chunk sizes"*. That reason does not
describe what the annotation actually permits: it also covers
`get_float64_column`'s `x as f64` on an `Int64` column (`:131`) and
`result.rs:310`, which are data-path conversions losing precision above 2^53 on
real user values.

The behavior is fine and matches `getInt64Column`'s documented caveat. Only the
justification is wrong. Narrow the allow to the specific diagnostic sites, or
widen the reason to name the data-path conversions honestly.

---

## Not in scope: separate investigations

- **macOS IPC is broken.** `BENCH_TRANSPORT=ipc` fails because `hyperd` never
  creates the Unix socket the client dials
  (`…/hyper-<pid>/domain/hyper`, ENOENT). It reproduces at the pre-migration
  baseline, so it predates the 1.88 uplift, and macOS IPC has never been
  captured in `BENCHMARK_GUIDE.md` — there is no recorded state it regressed
  from. Needs its own investigation, not a cleanup task.
- **The `qs` advisory cannot be fixed here.** Patched in 6.16.0, but the
  registry enforces a release-age cutoff making anything newer than 2026-08-29
  uninstallable, and every installable version (≤ 6.15.3) is affected. An
  `overrides` pin was tried and reverted because it broke `npm install`
  outright. Affects only the hyper-explorer example, which ships in no
  published package.
- **Float→integer saturation** still hands JS a plausible wrong number:
  `getInt32()` on a `DOUBLE` holding `5e9` returns `2147483647`. Deliberate and
  disclosed in rustdoc, `index.d.ts` and the changelog, but it is the same
  failure mode the narrowing work set out to eliminate. Worth an issue, not a
  fix in this plan — it is not a regression.
- **~134 markdown lint items** — 69 untagged code fences, 28 over-long lines,
  and heading-structure rules. Needs per-block judgment: an automated pass
  corrupted 176 fences by mistaking closing fences for opening ones, because a
  language-tagged opening fence does not match a bare-fence test. Any retry
  must track fence state.
- **`scrape_dc_sql_reference.py`** references `scripts/n.md` and `scrape_n.py`,
  which look like a sed/rename mishap in that script. Cosmetic, but it makes
  the documented refresh command wrong.

## Plan completion gate

1. Tasks 1–9 landed, each with captured output.
2. The full verification gate above exits 0 on every command.
3. CI green on all 17 checks, including `msrv (1.88)`, `doc`, and the RHEL job
   — with a `.rs` or `Cargo.toml` file touched so the path-filtered RHEL job
   actually runs.
4. Per-crate changelog bullets added for any public-API change (Task 1 if it
   propagates; Tasks 11–12 when the Node group lands).
5. `EXECUTION-LOG.md`-style record appended to this document with measurements,
   matching the practice established during the 1.88 uplift.
