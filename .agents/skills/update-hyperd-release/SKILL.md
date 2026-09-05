---
name: update-hyperd-release
description: Use when bumping the pinned hyperd release for hyperdb-bootstrap — finding the latest Tableau Hyper API version on PyPI, updating hyperd-version.toml (version + 4 wheel tags + 4 sha256s, all read straight off the PyPI JSON API), verifying the pin, running the full test suite, A/B benchmarking against the previous pin, logging the result per release, and opening the PR.
---

# Update the pinned `hyperd` release

Bumps the `hyperd` binary that `hyperdb-bootstrap` downloads, then proves the new
engine is correct and measures how it moved performance. Codifies the procedure in
[AGENTS.md](../../../AGENTS.md) ("Bootstrapping `hyperd`") plus the operational
gotchas learned in practice.

## Key facts (don't relearn these the hard way)

- **The pin lives in [`hyperdb-bootstrap/hyperd-version.toml`](../../../hyperdb-bootstrap/hyperd-version.toml)** — `version`, four `[wheel_tag]` entries, and four per-platform sha256s. That's the whole source of truth; contributors without an override get exactly this release. **There is no `build_id` any more.**
- **We download the PyPI `tableauhyperapi` wheels.** Every input to the URL is either the version you're bumping to or a value already in the pin, so nothing has to be discovered by scraping. The wheel carries `hyperd` at `tableauhyperapi/bin/hyper/hyperd` (`hyperd.exe` plus `crashdumper.exe` on Windows).
- **PyPI publishes a sha256 per file.** You *read* the four digests off the JSON API (step 2) instead of downloading four ~80 MB archives and hashing them by hand. This is the single biggest time saving in the whole procedure. The digests still get committed — a hash in git is an attestation that's independent of the host serving the bytes.
- **URL template:** `https://files.pythonhosted.org/packages/py3/t/tableauhyperapi/tableauhyperapi-<version>-py3-none-<wheel_tag>.whl` — this legacy path is constructible without an API call and 302-redirects to the content-addressed URL. Platform slugs: `macos-arm64`, `macos-x86_64`, `linux-x86_64`, `windows-x86_64`.
- **The wheel tags live in the pin because they are not guaranteed stable.** arm64 wheels only exist from `0.0.19484` onward, and a future macOS floor bump would change `macosx_13_0_arm64`. A wrong tag is a **silent 404**, not a loud error — so keeping the tags as pin data makes any such change a visible diff in the pin file. (Empirically all four tags are unchanged from `0.0.19484` through `0.0.26479`.)
- **Crate version is workspace-driven + release-please.** `hyperdb-bootstrap` uses `version.workspace = true`; do **not** hand-edit a crate version. The conventional-commit type drives the release — use `fix(bootstrap): ...` for a routine bump (patch release).
- **Never invent `hyperd` flags** (AGENTS.md reminder #9) and **never report tests/benches green without real output** (#10). Tests start a real `hyperd` subprocess; a misconfigured server hangs rather than erroring.

## Procedure

Track these as todos. Each step gates the next.

### 1. Create a branch

```bash
git checkout -b chore/bump-hyperd-<version>   # e.g. chore/bump-hyperd-0.0.26479
```

### 2. Read the version and all four digests off PyPI

Two commands. The first gives you the version to bump to; the second gives you
every wheel for that version with its published sha256, ready to paste into the
toml.

```bash
# Latest version on PyPI
curl -s https://pypi.org/pypi/tableauhyperapi/json | jq -r .info.version

# Every wheel + its published sha256 for that version — paste straight into the toml
curl -s https://pypi.org/pypi/tableauhyperapi/<VERSION>/json \
  | jq -r '.urls[] | "\(.filename)  \(.digests.sha256)"'
```

The second command prints four lines. Map filename → platform slug so each digest
lands on the right toml line:

| Wheel filename suffix          | Platform slug    | `[wheel_tag]` value      |
|--------------------------------|------------------|--------------------------|
| `macosx_13_0_arm64.whl`        | `macos-arm64`    | `macosx_13_0_arm64`      |
| `macosx_10_11_x86_64.whl`      | `macos-x86_64`   | `macosx_10_11_x86_64`    |
| `manylinux2014_x86_64.whl`     | `linux-x86_64`   | `manylinux2014_x86_64`   |
| `win_amd64.whl`                | `windows-x86_64` | `win_amd64`              |

**Confirm the four printed filenames still carry exactly those four tags.** A
changed tag is the silent-404 vector: the pin would still compile and `verify`
would be the only thing that catches it. If a tag *has* changed, update the
matching `[wheel_tag]` entry in the same edit as the digests.

Expect exactly four wheels. If PyPI lists more (or fewer) for the version, stop
and work out why before pinning it.

(The [`hyper-api-release-verify-upcoming-packages`](../hyper-api-release-verify-upcoming-packages/SKILL.md)
skill — bundled `verify_release.py` — validates downloadability and archive
integrity for a given `--version` if you want a second opinion.)

### 3. Edit `hyperd-version.toml`

Update `version` and all four `[sha256]` entries from the step-2 output; update a
`[wheel_tag]` entry only if step 2 showed the tag changed. Record the **old**
version first — you need it for the A/B benchmark (step 6).

The file should end up looking like this:

```toml
version = "0.0.26479"

[wheel_tag]
"macos-arm64"    = "macosx_13_0_arm64"
"macos-x86_64"   = "macosx_10_11_x86_64"
"linux-x86_64"   = "manylinux2014_x86_64"
"windows-x86_64" = "win_amd64"

[sha256]
"macos-arm64"    = "e80e4dac6d8437ad8c20f36add7e523b18bc06d90d4c605a256c57df8df2c118"
"macos-x86_64"   = "960e276028137847a3870695d9c2d5a1392c173b1e119ff1146d24a75deca71a"
"linux-x86_64"   = "9f5ff04c0dc3c17224b7a3f36f297775f2f49aae084da84614003cd6508213bc"
"windows-x86_64" = "7a4f96d2a22351e944fea6db5d03ab5272ad4c0577acc987bfcb3739ed639502"
```

### 4. Verify the pin

```bash
make verify-hyperd-pin                 # all four platforms → HTTP 200, and each
                                       # pinned sha256 matches the digest PyPI
                                       # publishes for that exact wheel filename
make download-hyperd                   # re-verifies the macos-arm64 sha256 on download
.hyperd/current/hyperd --version       # should report the new version
file .hyperd/current/hyperd            # sanity check: "Mach-O 64-bit executable
                                       # arm64" on Apple Silicon
```

`verify` cross-checking the digests, not just HEAD-ing the URLs, is what makes
this step meaningful: it proves the pin names the exact bytes PyPI serves, rather
than merely that the CDN serves *something* at that path.

### 5. Run the full test suite against the NEW engine

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

### 6. A/B benchmark vs the previous pin

The canonical harness is the **unified suite**
([`hyperdb-api/benches/benchmark_suite.rs`](../../../hyperdb-api/benches/benchmark_suite.rs)).
Download the **old** pin into a separate dir, then run the same suite on both.
See [docs/BENCHMARK_GUIDE.md](../../../docs/BENCHMARK_GUIDE.md) for the harness details.

`--version <OLD_VERSION>` on its own is enough for the baseline: it inherits the
builtin pin's `[wheel_tag]` values and carries no digests, so the download is
unverified and logs a WARN. That's fine for a throwaway baseline, and the four
tags are unchanged all the way back to `0.0.19484`. (If you ever need a baseline
from a release whose tags *do* differ, write a full pin file and use
`--version-file` instead.)

```bash
# Old engine into a scratch dir (sha256 skipped — that's fine for a throwaway baseline)
cargo run --release -p hyperdb-bootstrap --bin hyperdb-bootstrap -- \
  download --version <OLD_VERSION> --dest .hyperd-old

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

### 7. Log the release in the benchmark tracker

Append a row per engine to
[`docs/hyperd-release-benchmarks.md`](../../../docs/hyperd-release-benchmarks.md)
(median single-connection numbers + the machine + the caveat). This builds the
per-release history the BENCHMARK_GUIDE's by-platform tables don't capture.

### 8. Changelog

Add a `### Changed` bullet under `## [Unreleased]` in
[`hyperdb-bootstrap/CHANGELOG.md`](../../../hyperdb-bootstrap/CHANGELOG.md): the new
version, the wheel tags if any of them moved, and the headline performance A/B
(with the thermal caveat on multi-connection numbers). If `## [Unreleased]`
already has a `### Changed`, merge into it — a second sibling heading is
markdownlint MD024.

### 9. Commit + PR

- Commit with `git add <explicit files>` (never `-A`), type `fix(bootstrap): bump pinned hyperd to <version>`.
- **gh account:** the EMU account (`ssteiner_sfemu`) is Unauthorized on upstream. `gh auth switch --hostname github.com --user StefanSteiner`, then target upstream (it has the CI runners): `gh pr create --repo tableau/hyper-api-rust --base main --head StefanSteiner:<branch>`.
- Put the verification checklist + performance table in the PR body.

## Verification checklist (what "done" means)

- [ ] Four wheels listed on PyPI for the new version, tags matching the pin
- [ ] `make verify-hyperd-pin` → all four platforms HTTP 200 **and** all four digests match PyPI's published sha256
- [ ] `.hyperd/current/hyperd --version` reports the new version
- [ ] `file` confirms the macos-arm64 binary is native arm64
- [ ] `cargo test --workspace` → `failed=0` against the new engine
- [ ] `cargo fmt --check` + CI-exact `cargo clippy` clean
- [ ] A/B benchmark done (medians of ≥3 runs @ 100M rows); scratch `.hyperd-old` removed
- [ ] Row appended to `docs/hyperd-release-benchmarks.md`
- [ ] CHANGELOG `[Unreleased]` bullet added (merged into the existing `### Changed`)
- [ ] PR opened against `tableau/hyper-api-rust` from `StefanSteiner:<branch>`
