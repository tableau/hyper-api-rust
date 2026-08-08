---
name: update-hyperd-release
description: Use when bumping the pinned hyperd release for hyperdb-bootstrap — finding the latest Tableau Hyper API version, updating hyperd-version.toml (version + build_id + 4 sha256s), verifying the pin, running the full test suite, A/B benchmarking against the previous pin, logging the result per release, and opening the PR.
---

# Update the pinned `hyperd` release

Bumps the `hyperd` binary that `hyperdb-bootstrap` downloads, then proves the new
engine is correct and measures how it moved performance. Codifies the procedure in
[AGENTS.md](../../../AGENTS.md) ("Bootstrapping `hyperd`") plus the operational
gotchas learned in practice.

## Key facts (don't relearn these the hard way)

- **The pin lives in [`hyperdb-bootstrap/hyperd-version.toml`](../../../hyperdb-bootstrap/hyperd-version.toml)** — `version`, `build_id`, and four per-platform sha256s. That's the whole source of truth; contributors without an override get exactly this release.
- **We download the Java bundle, NOT the C++ one.** The C++ `macos-arm64` zip ships an **x86_64** `hyperd` (upstream packaging defect) that only runs under Rosetta on Apple Silicon. The Java `macos-arm64` bundle carries a **native arm64** `hyperd`. Same URL template (only the `java`/`cxx` token differs), same internal layout (`lib/hyper/hyperd`). **Verify this invariant every bump** (step 5) — if a future Java bundle regresses to x86_64, the whole reason for using it is gone.
- **URL template:** `https://downloads.tableau.com/tssoftware/tableauhyperapi-java-<platform>-release-main.<version>.<build_id>.zip` — platforms: `macos-arm64`, `macos-x86_64`, `linux-x86_64`, `windows-x86_64`.
- **Crate version is workspace-driven + release-please.** `hyperdb-bootstrap` uses `version.workspace = true`; do **not** hand-edit a crate version. The conventional-commit type drives the release — use `fix(bootstrap): ...` for a routine bump (patch release).
- **Never invent `hyperd` flags** (AGENTS.md reminder #9) and **never report tests/benches green without real output** (#10). Tests start a real `hyperd` subprocess; a misconfigured server hangs rather than erroring.

## Procedure

Track these as todos. Each step gates the next.

### 1. Create a branch

```bash
git checkout -b chore/bump-hyperd-<version>   # e.g. chore/bump-hyperd-0.0.26225
```

### 2. Find the latest version + build id

```bash
curl -sL "https://tableau.github.io/hyper-db/docs/releases" | rg -o "0\.0\.[0-9]+" | head -1
curl -sL "https://tableau.github.io/hyper-db/docs/releases" | \
  rg -o "tableauhyperapi-java-[a-z0-9_-]+-release-main\.<VERSION>\.r[a-z0-9]+\.zip" | sort -u
```

Confirm all four platform zips are listed for that version and share one build id.
(The [`hyper-api-release-verify-upcoming-packages`](../hyper-api-release-verify-upcoming-packages/SKILL.md)
skill — bundled `verify_release.py` — validates the whole page's downloadability and
zip integrity for a given `--version`.)

### 3. Compute the four sha256s

Download each Java zip and hash it. The values go verbatim into the toml.

```bash
V=<version>; B=<build_id>; cd "$(mktemp -d)"
for p in macos-arm64 macos-x86_64 linux-x86_64 windows-x86_64; do
  curl -sL --fail -o "$p.zip" \
    "https://downloads.tableau.com/tssoftware/tableauhyperapi-java-$p-release-main.$V.$B.zip" &
done; wait
for p in macos-arm64 macos-x86_64 linux-x86_64 windows-x86_64; do
  printf '%-16s ' "$p"; shasum -a 256 "$p.zip" | awk '{print $1}'
done
```

### 4. Edit `hyperd-version.toml`

Update `version`, `build_id`, and all four `[sha256]` entries. Record the **old**
version/build_id first — you need it for the A/B benchmark (step 7).

### 5. Verify the pin + the arm64 invariant

```bash
make verify-hyperd-pin                 # all four platforms → HTTP 200 at the new pin
make download-hyperd                   # re-verifies the macos-arm64 sha256 on download
.hyperd/current/hyperd --version       # should report main.<version>.<build_id>
file .hyperd/current/hyperd            # MUST say "Mach-O 64-bit executable arm64" on Apple Silicon
```

If `file` reports `x86_64`, **stop** — the Java bundle no longer carries a native
arm64 binary and the bundle choice needs re-evaluation.

### 6. Run the full test suite against the NEW engine

Point `HYPERD_PATH` at the freshly downloaded binary — do **not** rely on the
workstation default (`~/dev/bin/hyperd`), which may be an old or unversioned build.

```bash
export HYPERD_PATH="$PWD/.hyperd/current/hyperd"
cargo test --workspace 2>&1 | rg "test result:" | \
  awk '{p+=$4; f+=$6} END {print "TOTAL passed="p" failed="f}'
```

Require `failed=0`. Then the pre-commit gate: `cargo fmt --all -- --check` and
`cargo clippy --workspace --all-targets --all-features -- -D warnings` (CI's exact
clippy command).

### 7. A/B benchmark vs the previous pin

The canonical harness is the **unified suite**
([`hyperdb-api/benches/benchmark_suite.rs`](../../../hyperdb-api/benches/benchmark_suite.rs)).
Download the **old** pin into a separate dir, then run the same suite on both.
See [docs/BENCHMARK_GUIDE.md](../../../docs/BENCHMARK_GUIDE.md) for the harness details.

```bash
# Old engine into a scratch dir (sha256 skipped — that's fine for a throwaway baseline)
cargo run --release -p hyperdb-bootstrap --bin hyperdb-bootstrap -- \
  download --version <OLD_VERSION> --build-id <OLD_BUILD_ID> --dest .hyperd-old

cargo build -q -p hyperdb-api --release --example benchmark_suite
BIN=target/release/examples/benchmark_suite; ROWS=100000000   # 100M for signal over noise

# 3 runs each so you can take medians, not single noisy samples.
for i in 1 2 3; do HYPERD_PATH="$PWD/.hyperd-old/current/hyperd" "$BIN" $ROWS 4 2>&1 | rg "· " | rg "sync|async"; done
for i in 1 2 3; do HYPERD_PATH="$PWD/.hyperd/current/hyperd"     "$BIN" $ROWS 4 2>&1 | rg "· " | rg "sync|async"; done

rm -rf .hyperd-old   # clean up the scratch baseline (also add to .gitignore if you keep it)
```

**Benchmark caveats — do not skip:**
- **Use medians of ≥3 runs at 100M rows.** Single sub-second 10M-row runs have huge run-to-run variance; a "regression" at that size is usually noise (proven on the 0.0.26225 bump — a −20% insert delta at 10M vanished to −5–7% at 100M).
- **Distrust `× 4` / parallel numbers on a laptop.** They throttle thermally — throughput declines monotonically across sequential runs because the machine is hotter for the second engine. Report single-connection deltas as the reliable signal; withhold multi-connection deltas unless run on a cooled/pinned host.
- Report throughput as **M rows/s**, not wall time.

### 8. Log the release in the benchmark tracker

Append a row per engine to
[`docs/hyperd-release-benchmarks.md`](../../../docs/hyperd-release-benchmarks.md)
(median single-connection numbers + the machine + the caveat). This builds the
per-release history the BENCHMARK_GUIDE's by-platform tables don't capture.

### 9. Changelog

Add a `### Changed` bullet under `## [Unreleased]` in
[`hyperdb-bootstrap/CHANGELOG.md`](../../../hyperdb-bootstrap/CHANGELOG.md): the new
version/build, "verified native arm64", and the headline performance A/B (with the
thermal caveat on multi-connection numbers).

### 10. Commit + PR

- Commit with `git add <explicit files>` (never `-A`), type `fix(bootstrap): bump pinned hyperd to <version> (<build_id>)`.
- **gh account:** the EMU account (`ssteiner_sfemu`) is Unauthorized on upstream. `gh auth switch --hostname github.com --user StefanSteiner`, then target upstream (it has the CI runners): `gh pr create --repo tableau/hyper-api-rust --base main --head StefanSteiner:<branch>`.
- Put the verification checklist + performance table in the PR body.

## Verification checklist (what "done" means)

- [ ] `make verify-hyperd-pin` → all four platforms HTTP 200
- [ ] `.hyperd/current/hyperd --version` reports the new version/build
- [ ] `file` confirms macos-arm64 binary is native arm64
- [ ] `cargo test --workspace` → `failed=0` against the new engine
- [ ] `cargo fmt --check` + CI-exact `cargo clippy` clean
- [ ] A/B benchmark done (medians of ≥3 runs @ 100M rows); scratch `.hyperd-old` removed
- [ ] Row appended to `docs/hyperd-release-benchmarks.md`
- [ ] CHANGELOG `[Unreleased]` bullet added
- [ ] PR opened against `tableau/hyper-api-rust` from `StefanSteiner:<branch>`
