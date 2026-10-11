# GitHub Operations

How this repo uses GitHub: what runs on every push and PR, what runs on
tag pushes, how releases become crates.io publishes and downloadable
binaries, and what maintainers do by hand vs. what the automation does.

Audience: maintainers and contributors who want to know "what happens
when I push", "how do I cut a release", or "where do the pre-built
binaries on the Releases page come from".

## Repository

- **Canonical URL:** <https://github.com/tableau/hyper-api-rust>
- **Default branch:** `main`
- **License:** dual MIT / Apache-2.0 (see [LICENSE-MIT.txt](../LICENSE-MIT.txt), [LICENSE-APACHE.txt](../LICENSE-APACHE.txt))
- **Governance:** see [CONTRIBUTING.md](../CONTRIBUTING.md) for the
  do-acracy / meritocracy model, PR workflow, and contribution checklist.

## Workflows

Seven GitHub Actions workflows live under [`.github/workflows/`](../.github/workflows/):

| Workflow | File | Triggers | Purpose |
|---|---|---|---|
| `ci` | [ci.yml](../.github/workflows/ci.yml) | `push` to `main`, all PRs, manual | fmt, clippy, msrv, doc, full test matrix, Node bindings build, `cargo deny`, `cargo audit`, version consistency, `cargo publish --dry-run` |
| `release-please` | [release-please.yml](../.github/workflows/release-please.yml) | `push` to `main`, GitHub Release `published`, manual | open/update the release PR with version bumps + CHANGELOG. Does **not** tag: `skip-github-release: true` means the maintainer creates the tag and Release by hand — see [Cutting a release](#cutting-a-release). The `release: published` re-run re-anchors the next `-rc.N` on the just-cut tag (#308) |
| `release` | [release.yml](../.github/workflows/release.yml) | GitHub Release `published`, manual (`workflow_dispatch` against an existing tag) | check that CI already passed on the tagged SHA, publish the 8 Rust crates to crates.io (`hyperdb-api-node` is published separately to npm). **Not** a tag push: that trigger was removed to stop duplicate runs, so pushing a tag alone publishes nothing |
| `npm-build-publish` | [npm-build-publish.yml](../.github/workflows/npm-build-publish.yml) | GitHub Release published, manual | build npm platform packages with bundled hyperd, publish to npm registry |
| `verify-hyperd-pin` | [verify-hyperd-pin.yml](../.github/workflows/verify-hyperd-pin.yml) | changes to `hyperdb-bootstrap/hyperd-version.toml` or its source, weekly cron, manual | `HEAD` every pinned hyperd release URL to catch Tableau yanks / typos |
| `verify-release-pr-version` | [verify-release-pr-version.yml](../.github/workflows/verify-release-pr-version.yml) | `pull_request` to `main` | fail the release-please PR if it proposes a version behind `main` (backward-bump guard, #308). Runs on every PR but only acts on the `release-please--branches--*` branch, so non-release PRs report a plain `success` |
| `rhel-compatibility` | [rhel-compatibility.yml](../.github/workflows/rhel-compatibility.yml) | `push` to `main` and PRs touching Rust/manifests/toolchain config, manual | `cargo check --workspace --locked --all-targets` in a `ubi9/ubi` container using RHEL's `rust-toolset` and no rustup — the M-OOBE enforcement |

### CI (`ci.yml`)

Runs on **every PR** and on **every push to `main`**. Jobs:

- `rustfmt` — `cargo fmt --all --check`.
- `clippy (ubuntu-24.04)` and `clippy (windows-2025)` — `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- `msrv (1.88)` — `cargo check` of the workspace and `hyperdb-compile-check` at the MSRV floor.
- `doc` — `cargo doc --no-deps` with `RUSTDOCFLAGS="-D warnings"` for the eight workspace crates (mirrors `make doc`); `hyperdb-compile-check` sits outside the workspace and is not covered.
- `test (ubuntu-24.04)`, `test (macos-26)`, `test (windows-2025)` — full workspace test matrix.
- `hyperdb-api-node (build + smoke)` — builds the native addon and runs a smoke test.
- `publish dry-run` — `cargo publish --dry-run` for `hyperdb-bootstrap` and `sea-query-hyperdb` only; the other crates depend on sibling versions that exist only after the release wave lands, so their dry-run cannot resolve.
- `cargo-deny` — license and advisory policy enforcement per [`deny.toml`](../deny.toml).
- `cargo-audit` — RustSec advisories, `--deny warnings`.
- `version consistency` — `version.txt` matches the Cargo workspace version, and the npm `package.json` files carry no in-source `version` field (`npm-build-publish.yml` injects it at publish time).

In-progress PR CI runs are **cancelled** when a new commit is pushed to
the PR. Main-branch runs always complete. This is set via the
`concurrency` block at the top of [ci.yml](../.github/workflows/ci.yml).

### Release (`release.yml`)

Runs on the **`release: published`** event (a maintainer publishes the
GitHub Release by hand after merging the release PR — `skip-github-release`
is `true`) or via manual `workflow_dispatch` with an explicit tag input
(for re-runs or emergency releases). Structure:

```text
verify          ← polls the CI check-runs on the tagged SHA + hyperd URL check
   │
   └─► publish          ← crates.io publish in dependency order
```

There is no per-platform binary-archive build today. Users on supported
platforms install via crates.io (`cargo install hyperdb-mcp`) or via npm
(`npm install -g hyperdb-mcp`, which delivers prebuilt binaries through
`npm-build-publish.yml`). The crates themselves are
architecture-independent source on crates.io.

**Dependency-ordered crates.io publish** (per-crate `sleep 45` between
each so the crates.io index has time to settle before the next crate's
verification step resolves the just-published dep):

1. `hyperdb-api-salesforce` (no workspace runtime deps; published first to
   break the optional cycle with `hyperdb-api-core`)
2. `hyperdb-api-core`
3. `hyperdb-api`
4. `hyperdb-compile-check` (outside the workspace; published with `--allow-dirty`)
5. `hyperdb-api-derive`
6. `hyperdb-mcp`
7. `hyperdb-bootstrap`
8. `sea-query-hyperdb`

`hyperdb-api-node` is **not** on the crates.io list (its `Cargo.toml` has
`publish = false`) — it ships as npm `hyperdb-api-node` through napi-rs's
own pipeline, which is outside this workflow today.

**Pre-release vs. stable:** the GitHub Release is marked `prerelease: true`
automatically for tags containing `-rc.`, `-alpha.`, or `-beta.` — they
show up on the Releases page but are not flagged as "Latest release".

**Concurrency:** only one release workflow runs at a time (the `concurrency:
release` group at the top of the file); a second release event during a
release will queue, not clobber.

### npm-publish (`npm-build-publish.yml`)

Builds and publishes npm packages for `hyperdb-mcp` and `hyperdb-api-node`
with the `hyperd` database engine bundled into each platform package. This
lets end users run `npx hyperdb-mcp` or `npm install hyperdb-api-node`
without needing Rust toolchains or manual hyperd setup.

**Triggers:**

- **GitHub Release published** — fires on the same `release: published`
  event as `release.yml`; the two workflows run in parallel.
- **Manual `workflow_dispatch`** — tag/branch input is optional; leave
  empty to build from the default branch HEAD (useful for testing the
  pipeline without tagging).

**Structure:**

```text
verify-ci       ← checks that CI passed for this commit (gh api check-runs)
   │
   └─► build-npm (matrix × 3 platforms)
          build hyperdb-mcp + hyperdb-api-node native binaries
          download hyperd via curl with SHA256 verification
          assemble platform packages (binary + hyperd + LICENSE-HYPERD)
          upload as GitHub Actions artifacts (7-day retention)
              │
              └─► publish-npm
                    publish platform packages then main packages to npm
```

**Platform matrix:**

| Platform | Runner | Rust target | hyperd source |
|---|---|---|---|
| `darwin-arm64` | `macos-26` | `aarch64-apple-darwin` | `macos-arm64` |
| `linux-x64-gnu` | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` | `linux-x86_64` |
| `win32-x64-msvc` | `windows-2025` | `x86_64-pc-windows-msvc` | `windows-x86_64` |

`darwin-x64` (Intel macOS) is currently disabled — `macos-13` GHA
runners have been unreliable. The matrix entry is commented out in
[`npm-build-publish.yml`](../.github/workflows/npm-build-publish.yml)
and will be re-enabled when runner availability stabilizes. Until then,
Intel-Mac users must build from source.

**npm packages published:**

| Package | Type | Contents |
|---|---|---|
| `hyperdb-mcp` | Main (bin shim) | `bin.js` — detects platform, sets `HYPERD_PATH`, spawns native binary |
| `hyperdb-mcp-darwin-arm64` | Platform | `hyperdb-mcp` + `hyperd` + `LICENSE-HYPERD` |
| `hyperdb-mcp-linux-x64-gnu` | Platform | same, Linux x64 |
| `hyperdb-mcp-win32-x64-msvc` | Platform | same, Windows x64 |
| `hyperdb-api-node` | Main (napi-rs) | JS bindings + `getHyperdPath()` helper |
| `hyperdb-api-node-*` | Platform | `.node` addon + `hyperd` + `LICENSE-HYPERD` |

**CI gate:** On a `release: published` event the `verify-ci` job polls the
tagged commit's CI check-runs and aborts the workflow if a required check
failed. A manual `workflow_dispatch` skips `verify-ci` and builds without
that gate. Note: this does **not** prevent tagging — git tags can be
created regardless of CI status. Use GitHub Rulesets (repo Settings →
Rules) to enforce tag-creation restrictions if needed.

**Downloading artifacts without publishing:** Since `publish-npm`
requires `NPM_TOKEN`, you can trigger a manual dispatch to test the
build — the build jobs will succeed and upload downloadable artifacts,
while `publish-npm` fails harmlessly.

```bash
# Trigger build from current main (no tag needed)
gh workflow run npm-build-publish.yml

# Trigger build for a specific tag
gh workflow run npm-build-publish.yml --field tag=v0.1.0

# Download artifacts after the run completes
gh run download <run-id> --name npm-darwin-arm64
```

**Local builds:** Use `make npm-pack` to build the current platform's
npm packages locally without CI. This produces `.tgz` files you can
share directly:

```bash
make npm-pack
npm install -g ./hyperdb-mcp/npm/hyperdb-mcp-darwin-arm64-*.tgz \
               ./hyperdb-mcp/npm/hyperdb-mcp-*.tgz
```

### verify-hyperd-pin (`verify-hyperd-pin.yml`)

Independently checks that the per-platform URLs baked into
[`hyperdb-bootstrap/hyperd-version.toml`](../hyperdb-bootstrap/hyperd-version.toml)
still resolve (via `hyperdb-bootstrap verify`). Runs:

- On any PR that touches the pin file or `hyperdb-bootstrap/src/**` (early-warn before the pin change lands).
- On push to `main` for the same paths (covers the merge).
- Every Monday at 12:00 UTC regardless of PR traffic (catches Tableau
  yanking a release out from under us).
- Manually via `workflow_dispatch`.

## Cutting a release

Releases are driven by [release-please](https://github.com/googleapis/release-please).
Maintainers don't bump versions or edit changelogs by hand — those steps are
automated from [Conventional Commits](https://www.conventionalcommits.org/).
The tag and GitHub Release **are** created by hand; see step 5.

### How it flows

1. Contributors land PRs into `main` with conventional-commit titles
   (`feat:`, `fix:`, `chore:`, etc. — see
   [CONTRIBUTING.md](../CONTRIBUTING.md#commit-message-format)).
2. The [release-please workflow](../.github/workflows/release-please.yml)
   runs on every push to `main`. It opens (or updates) a single
   **release PR** titled `chore(main): release X.Y.Z`. That PR contains:
   - `Cargo.toml`'s `[workspace.package] version`, which all 8 workspace
     members inherit via `version.workspace = true`, plus out-of-workspace
     `hyperdb-compile-check`'s own `version` — 9 path crates in all.
   - The inter-crate `= "X.Y.Z"` pins, updated in place between the
     `x-release-please-start-version` markers.
   - A new dated section in the **root** [`CHANGELOG.md`](../CHANGELOG.md)
     summarizing the conventional commits that landed since the last release.
   - An updated `.release-please-manifest.json` and `version.txt`.
   - Both lockfiles, resynced by a follow-up workflow step.

   It does **not** touch the nine per-crate `CHANGELOG.md` files, and it does
   **not** touch any `package.json` — see
   [Per-crate changelogs](#per-crate-changelogs-are-not-automated) and
   [npm versions](#npm-versions-are-stamped-at-publish-time-not-in-the-release-pr).
3. A maintainer reviews the release PR. Adjust the version manually if a
   different bump is needed (e.g., promote a `0.x.0` patch to a minor) by
   editing the PR or by tagging commits with
   [`Release-As: X.Y.Z`](https://github.com/googleapis/release-please?tab=readme-ov-file#how-can-i-fix-release-notes).
   **Read the version in the PR title before merging** — it is the last check
   before anything becomes permanent.

   **While the rc line is open**, that version bumps itself to the next
   `-rc.N` with no footer needed: release-please anchors on the previous
   rc's tag, which exists on every ordinary push. The one gap — right after
   the release PR merges but before you cut the new tag — is closed by
   re-running release-please on the `release: published` event when you cut
   it (#308), and the [backward-version guard](#the-backward-version-guard) fails the
   release PR if that anchoring ever slips. See [Pre-releases](#pre-releases).
   **Before shipping `1.0.0` final, remove the prerelease keys from
   [release-please-config.json](../release-please-config.json)** — leave them
   in and the release after `1.0.0` computes `1.0.1-rc`. See
   [Graduating to `1.0.0`](#graduating-to-100).
4. **Merge the release PR.** This lands the version bumps and changelogs on
   `main`. It does **not** tag: `skip-github-release` is `true` in
   `release-please-config.json`, so release-please creates neither the tag nor
   the GitHub Release.
5. **Create the tag and Release by hand.** See
   [Manual tag step](#manual-tag-step-after-release-please-pr-merge) below. This is the step that actually
   starts a publish, so skipping it leaves the release stalled with everything
   merged and nothing shipped.
6. **Publish workflows fire from the Release.** `gh release create` emits
   `release: published`, which triggers both `release.yml` (crates.io) and
   `npm-build-publish.yml` (npm). It also re-runs `release-please.yml`, which
   now anchors on the just-created tag and regenerates the open release PR
   with the correct next `-rc.N` (#308). Because the Release is created with a
   PAT rather than the default `GITHUB_TOKEN`, those triggers are not
   suppressed. The npm workflow waits for CI to pass before building.

   If a publish workflow fails and needs a re-run:

   ```bash
   gh workflow run release.yml -f tag=vX.Y.Z
   gh workflow run npm-build-publish.yml -f tag=vX.Y.Z
   ```

   Already-published crates are skipped gracefully on re-run.

### Manual tag step (after release-please PR merge)

In practice the tag is created **by hand**, not by release-please. Merging
the `chore(main): release X.Y.Z` PR bumps the manifest, `Cargo.toml`
versions, and `CHANGELOG.md` on `main` — but a maintainer then inspects
the merged commit and creates the `vX.Y.Z` tag + GitHub Release
themselves. This is a deliberate human checkpoint: once the tag exists,
`release.yml` fires automatically and publishes to crates.io, where
versions are permanent (`cargo yank` only hides a version — it never
frees the number), and npm is effectively the same. The manual tag is
the last point at which a bad release can be stopped; everything before
it is reversible, everything after it is not.

```bash
# 1. Fetch and confirm main is at the expected merge SHA.
git fetch upstream main
git log -1 upstream/main --format="%H %s"
# Should show: <merge-sha> chore(main): release X.Y.Z (#NN)

# 2. Sanity-check the manifest matches what you expect to release.
gh api repos/tableau/hyper-api-rust/contents/.release-please-manifest.json?ref=<merge-sha> \
  --jq '.content' | base64 -d
# Should show: { ".": "X.Y.Z" }

# 3. Extract the release notes from the new CHANGELOG.md section.
#    awk between the two H2 anchors, then drop the H2 line and the blank
#    line under it (tail -n +3) and the trailing blank that abuts the
#    next section (sed '$d').
awk '/^## \[X\.Y\.Z\]/,/^## \[<previous>\]/' CHANGELOG.md \
  | sed '$d' | tail -n +3 > /tmp/vX.Y.Z-notes.md

# 4. Promote the release PR's label to `autorelease: tagged` BEFORE creating
#    the Release. Creating the Release (next step) fires `release: published`,
#    which re-runs release-please to re-anchor the next -rc.N (#308). Promoting
#    the label first guarantees that re-run won't abort on "untagged, merged
#    release PRs outstanding". This is only a label edit — reversible — so the
#    point of no return is still `gh release create` below.
gh pr edit <release-pr-number> -R tableau/hyper-api-rust \
  --remove-label "autorelease: pending" \
  --add-label "autorelease: tagged"

# 5. Create the tag + GitHub Release. --target accepts the merge SHA; the
#    positional arg is the tag name. Both release.yml and release-please.yml
#    fire on the resulting `release: published` event — release.yml publishes
#    to crates.io, release-please re-anchors the next rc PR (#308).
gh release create vX.Y.Z \
  -R tableau/hyper-api-rust \
  --target <merge-sha> \
  --title "vX.Y.Z" \
  --notes-file /tmp/vX.Y.Z-notes.md \
  --latest    # OR --prerelease for -rc / -alpha / -beta tags
```

After the tag is created:

1. Watch [`release.yml`](https://github.com/tableau/hyper-api-rust/actions/workflows/release.yml) —
   it confirms CI already passed on the tagged SHA, then publishes the
   crates to crates.io in dependency order.
2. Watch [`npm-build-publish.yml`](https://github.com/tableau/hyper-api-rust/actions/workflows/npm-build-publish.yml)
   in parallel.
3. Confirm the new version landed (see [Verifying a release](#verifying-a-release)).
4. **Roll over the per-crate changelogs** — see
   [Rolling over the per-crate changelogs](#rolling-over-the-per-crate-changelogs).
   Nothing automates this, and skipping it is silent.

**To stop a release after the PR merges but before tagging.** If you find
a problem in the merged release PR before creating the tag, nothing has
shipped yet — you have options:

- **Land a fix on `main`.** release-please opens a fresh
  `chore(main): release X.Y.Z` PR rolling the fix into the next version.
  Promote the old PR's label to `autorelease: snooze` so the original
  version isn't re-proposed (only when the new release will carry a
  different version number).
- **Force a `Release-As` bump** to skip the bad version with an empty
  commit on `main`:

  ```bash
  git commit --allow-empty -m "chore: release X.Y.(Z+1)" -m "Release-As: X.Y.(Z+1)"
  ```

  release-please then opens a fresh PR for that version.
- **Revert the release PR's commit on `main`** if the bump itself is
  wrong, fix the manifest by hand if needed, and let release-please
  reconcile on the next run.

Two things worth knowing here. First, a *forgotten* rc bump is not one of
these cases: because release-please re-runs on `release: published` and every
ordinary push already has the previous tag present, the next `-rc.N` computes
itself — the `Release-As:` footer is for pinning a *different* version (skip a
burned number, jump straight to `1.0.0`), not for the routine next rc. Second,
step 4 of the runbook promotes the release PR's label to `autorelease: tagged`
*before* the tag is cut, so if you stop here — after promoting but before
`gh release create` — nothing has shipped; just re-label the PR
(`autorelease: snooze`, per the first bullet).

### Rolling over the per-crate changelogs

release-please does not touch the nine per-crate `CHANGELOG.md` files (see
[Per-crate changelogs are not automated](#per-crate-changelogs-are-not-automated)),
so after a release ships, every bullet contributors added under
`## [Unreleased]` still sits there describing work that is now published.

Once the tag exists, open a follow-up `docs:` PR that, for each crate whose
`## [Unreleased]` section is non-empty:

1. Renames `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, keeping its
   `### Added` / `### Changed` / `### Fixed` subsections in
   [Keep a Changelog](https://keepachangelog.com/) order.
2. Inserts a fresh, empty `## [Unreleased]` above it.

Leave crates with an empty `## [Unreleased]` alone — not every crate changes
in every release, and an empty dated section is noise.

Two things to watch:

- Run `npx markdownlint-cli2` afterwards. The most common failure is **MD024**
  (duplicate sibling headings) when the new dated section ends up next to an
  existing one with the same `### Fixed` / `### Added` subheading.
- Do this as its own PR. Bundling it with feature work makes the release
  bookkeeping invisible in review.

This is deliberately manual rather than delegated to release-please. Handing
all nine files to release-please would mean adding nine packages to
[release-please-config.json](../release-please-config.json), which is exactly
the multi-package layout the [lockstep](#lockstep-versioning) setup avoids —
a large change to a working release pipeline in exchange for bookkeeping that
takes minutes per release.

### How commits drive version bumps

release-please reads the [Conventional Commits](https://www.conventionalcommits.org/)
prefix on each commit since the last release tag and picks the largest
bump implied. Mark a commit as a breaking change by either appending `!`
after the type (e.g. `feat!:`) or by adding a `BREAKING CHANGE:` footer
in the commit body.

Now that the workspace is on `1.x`, standard semver applies: a breaking
change bumps the **major** component. This differs from the `0.x` line,
where release-please mapped a breaking change to a minor bump because
semver treats all of `0.x` as unstable — so bump examples written before
1.0.0 do not carry over.

| Commit prefix on `main` | Bump from `1.0.0` to |
|---|---|
| `fix:`, `fix(scope):` | `1.0.1` (patch) |
| `feat:`, `feat(scope):` | `1.1.0` (minor) |
| `feat!:` / `fix!:` / `BREAKING CHANGE:` footer | `2.0.0` (major) |
| `chore:`, `docs:`, `refactor:`, `test:`, `style:`, `ci:`, `perf:`, `build:` | no release |
| Manual `Release-As: X.Y.Z` footer | exactly `X.Y.Z` (overrides the computed bump) |

After the workspace is on `1.x.y`, the same prefixes follow normal
semver: `feat!:` will bump `1.2.3` → `2.0.0` as expected.

> **While the rc line is open, that table does not apply.** The config sets
> `"versioning": "prerelease"` and `"prerelease": true`, so every releasing
> prefix above collapses to the *next rc* instead of moving
> `major.minor.patch`: measured from `1.0.0-rc.2`, each of `fix:`, `feat:`, and
> `feat!:` computes `1.0.0-rc.3`. The table describes what happens once those
> keys come back out. JSON takes no comments, so this note is the only place
> that pairing is written down — see [Pre-releases](#pre-releases) and
> [Graduating to `1.0.0`](#graduating-to-100).

`bump-minor-pre-major` used to appear twice in
[release-please-config.json](../release-please-config.json) (top level and
inside the `.` package). It was **removed** alongside the prerelease keys.
release-please only consults it when `version.isPreMajor` — defined as
`major < 1` — and the workspace has been at major `1` since `1.0.0-rc.1`, so
neither copy had any effect after that point. It read as protection against an
accidental major bump while providing none. Dry runs confirmed removing it
changes no computed version, under either the old or the new config. Don't
re-add it: at `1.x` it cannot do anything except mislead the next reader.

To stabilize the API and cut `1.0.0`, flip `"prerelease"` to `false` in
[release-please-config.json](../release-please-config.json) — the full
procedure and its alternatives are in
[Graduating to `1.0.0`](#graduating-to-100). A `Release-As: 1.0.0` footer on a
conventional-commit on `main` also produces `1.0.0` without touching the
config:

```text
feat: stabilize public API

Release-As: 1.0.0
```

The footer alone is **not sufficient**, though. It pins that one release to
`1.0.0` and leaves the prerelease keys in place, so the *next* `fix:` computes
`1.0.1-rc`. However you cut `1.0.0`, the keys still have to come out.

### Pre-releases

While the repo is on an `-rc.N` line, **the rc counter increments by itself**.
Three keys on the `.` package in
[release-please-config.json](../release-please-config.json) put release-please
in prerelease mode:

```json
"prerelease": true,
"prerelease-type": "rc",
"versioning": "prerelease"
```

With those set, `PrereleaseVersioningStrategy` handles the bump: while the
current version carries a prerelease **and** the `major.minor.patch` part is
unchanged, every conventional commit increments the prerelease counter instead
of moving the version triple. Measured from `1.0.0-rc.2`, every releasing
prefix computes `1.0.0-rc.3` — no footer, no maintainer action:

| Highest-precedence commit since the tag | Computed version |
|---|---|
| `fix:` | `1.0.0-rc.3` |
| `feat:` | `1.0.0-rc.3` |
| `feat!:` / `fix!:` / `BREAKING CHANGE:` | `1.0.0-rc.3` |
| any of the above **+ `Release-As: X.Y.Z`** | exactly `X.Y.Z` |

This "no footer, no maintainer action" bump depends on release-please anchoring
on the previous rc's **tag**, so that tag has to exist when release-please runs.
On every ordinary push it does. The one window where it doesn't — right after
the release PR merges but before the maintainer hand-cuts the new tag — is
covered by re-running release-please on the `release: published` event the tag
emits (#308); the [backward-version guard](#the-backward-version-guard) fails the release PR
if the anchoring ever slips regardless.

Pre-release tags flow through `release.yml` and `npm-build-publish.yml` exactly
as stable releases do; the GitHub Release is flagged as `prerelease: true`, and
the npm `dist-tag` is set to `rc` / `alpha` / `beta` instead of `latest` so
`npm install hyperdb-mcp` doesn't pull a pre-release by default.

#### `Release-As:` is an override, not the routine path

A `Release-As: X.Y.Z` footer works and always wins: `determineReleaseType`
looks for a `RELEASE AS` note first and short-circuits before any prerelease
logic runs. Reach for it to pin one specific version — skipping a burned
number, or jumping straight to `1.0.0` — rather than to produce the next rc,
which happens on its own.

**When you do use the footer, it has to land in the squash commit body.**
Recent PRs are squash-merged, and a footer that exists only on a branch commit
is discarded. (`v1.0.0-rc.2`'s footer survived because that PR got a real merge
commit.) Alternatively an admin can push a direct empty commit, since
`enforce_admins` is false on `main`:

```bash
git commit --allow-empty -m "chore: release 1.0.0-rc.4" -m "Release-As: 1.0.0-rc.4"
```

An already-open release PR **self-corrects in place** whenever anything lands
on `main` — release-please recomputes and force-pushes its branch on the next
run. Don't close or hand-edit it. After a manual-tag cut, that "next run" is the
one the `release: published` event triggers (#308); because the runbook now
promotes the release PR's label to `autorelease: tagged` before cutting the tag
(see [Manual tag step](#manual-tag-step-after-release-please-pr-merge)), that
re-run anchors on the new tag and corrects the PR immediately rather than
aborting.

#### Reading the prerelease keys against the schema

`prerelease` is an overloaded key. The config schema documents it as "create
the GitHub release as prerelease", but `PrereleaseVersioningStrategy` also
reads it to decide whether to keep the prerelease suffix or truncate it. Here
the release-flag meaning is moot: `skip-github-release: true` means
release-please never creates the GitHub Release at all — a maintainer does
that by hand, passing `--prerelease` to `gh release create` — so the key acts
purely as the versioning switch.

All three keys are valid at the top level *and* inside the `.` package. The
root of the config schema pulls in the same `ReleaserConfigOptions` that each
package entry uses, and `mergeReleaserConfig` resolves every field as
`package ?? top-level ?? built-in default`. Dry runs confirmed all three
placements (package-only, top-level-only, both) compute the same version. They
live on the `.` package here because that is the winning side of that merge,
and because one location means one place to edit at `1.0.0` instead of two.

#### The backward-version guard

[`verify-release-pr-version.yml`](../.github/workflows/verify-release-pr-version.yml)
is the CI backstop for the anchoring above. On every PR to `main` it compares the
proposed `.release-please-manifest.json` version against the one already on
`main` and **fails the check if the PR moves the version backward**. It acts only
on the `release-please--branches--*` branch — every other PR reports a plain
`success` — so it is safe to require (see [Branch protection](#branch-protection)).

This is the guard against the #308 regression. If release-please ever computes a
next `-rc.N` behind an already-published version — a stale `Release-As:` footer in
history can force this when it runs before the tag exists — the guard turns what
used to be a silent, unrecoverable backward publish into a loud, pre-merge CI
failure. To clear it, re-run release-please once the previous release's tag
exists (it re-runs on its own on `release: published`, #308), or add a
`Release-As:` footer pinning the correct forward version.

The guard survives the `1.0.0` graduation unchanged: `1.0.0` sorts *above*
`1.0.0-rc.N`, so graduating forward passes, while a stale
`Release-As: 1.0.0-rc.N` footer trying to drag a post-1.0.0 line back fails. Its
semver comparator is covered by
[`test_verify_release_pr_version.py`](../.github/scripts/test_verify_release_pr_version.py).

#### Graduating to `1.0.0`

**The prerelease keys must come out when `1.0.0` final ships.** This is the one
cost the automation adds, and it is not optional.

Left in place at a non-prerelease version, `PrereleaseVersioningStrategy` takes
its other branch: with no prerelease suffix to increment, it bumps the triple
and *attaches* `prerelease-type` instead. So the first `fix:` after `1.0.0`
computes **`1.0.1-rc`** — verified by dry run — and every subsequent stable
release becomes an rc until someone edits the config.

Either edit closes the rc line:

- **Flip `"prerelease"` to `false`**, leaving the other two keys. The strategy
  computes the bump and then truncates the prerelease, so the very next release
  is `1.0.0`. A one-character graduation.
- **Delete all three keys.** Restores the plain default strategy and makes the
  table in [How commits drive version bumps](#how-commits-drive-version-bumps)
  accurate again. Prefer this once `1.0.0` has shipped and the rc line is
  closed for good.

**Why this trade is worth taking.** Both setups can put a wrong version in the
release PR; they fail *differently*, and that is the whole point:

| | Before (footer on every rc) | After (automatic rc bump) |
|---|---|---|
| Correct human action needed | on **every** rc release | **once**, at `1.0.0` |
| Version if forgotten | `1.0.1-rc.2` | `1.0.1-rc` |
| How you find out | nothing objects | wrong version in the PR title |
| Recoverable? | **no**, once published | yes, before merging |

The old failure was silent *and* terminal. `v1.0.1-rc.2` is well-formed: it
satisfies the tag regex in both publish workflows and it matches the
`Cargo.toml` the release PR itself wrote, so the tag-vs-manifest guard agrees
and nothing in CI objects. And because `1.0.1-rc.2` sorts *above* `1.0.0`,
publishing it would make a later `1.0.0` final a downgrade, permanently
forfeiting the ability to finish the rc line — `cargo yank` hides a version but
never frees the number.

The new failure is loud and cheap: `1.0.1-rc` appears in the release PR title
before anything is tagged or published, and the fix is the config edit that was
missed. The tripwire moved from silent-and-unrecoverable to
visible-and-reversible. It did not disappear, which is why reading the version
in the release PR title before merging is still the last check.

The [backward-version guard](#the-backward-version-guard) does **not** catch this
particular slip — `1.0.1-rc` sorts *above* `1.0.0`, so it is a *forward* move, and
the guard only rejects backward ones. That is by design: the guard's job is the
opposite failure (#308), and it correctly *passes* the legitimate graduation
because `1.0.0` sorts above every `1.0.0-rc.N`. Reading the PR title remains the
check that catches a forgotten config edit.

#### Re-verifying the prerelease behaviour

If release-please is upgraded, or the config is edited again, re-run the dry
runs rather than trusting this section. Three things make that harder than it
looks:

- `release-please-action@v5` bundles release-please **17.6.0** — `dist/index.js`
  at the `v5` tag carries `VERSION = '17.6.0'`. The action's `package.json`
  only declares `^17.6.0`, so the range is not the pin; read the bundle.
- `release-pr` has **no `--prerelease` flag** (only `github-release` does), so
  `prerelease: true` can only be exercised through a real config file.
- `--local` / `--local-path` does **not** read the config from the local clone.
  Config and manifest always come over the API from the target branch, so a
  variant has to be committed and pushed to a branch somewhere.

Which means: push the candidate config to a scratch branch (a fork is fine as
long as the last release tag exists there), then point a dry run at it.

```bash
npx release-please@17.6.0 release-pr \
  --repo-url=<owner>/<repo> \
  --target-branch=<scratch-branch> \
  --config-file=release-please-config.json \
  --manifest-file=.release-please-manifest.json \
  --token="$(gh auth token)" \
  --dry-run
```

Validate the harness before trusting it: run the *current* config against
`main` first and confirm it reproduces the version in the open release PR. If
it doesn't, nothing downstream counts.

The same harness re-checks the #308 fix. Push the candidate config to a scratch
branch **that carries the last release's tag**, then confirm a dry run computes
the correct *next* `-rc.N` rather than a backward one. When re-checking the
manual-tag runbook, verify the release PR's label is promoted to
`autorelease: tagged` *before* `gh release create` (see
[Manual tag step](#manual-tag-step-after-release-please-pr-merge)) so the
`release: published` re-run anchors instead of aborting. The
[backward-version guard](#the-backward-version-guard)'s own comparator has unit
tests: `python3 .github/scripts/test_verify_release_pr_version.py`.

### Lockstep versioning

All **9 path crates** — the 8 workspace members plus out-of-workspace
`hyperdb-compile-check` — share a single version number. When any crate's
commits trigger a bump, every crate moves together, which keeps
`cargo publish`'s strict inter-crate pins (`= "X.Y.Z"`) in sync without
manual edits.

There is **no `linked-versions` plugin**, and no `plugins` key at all, in
[release-please-config.json](../release-please-config.json). Lockstep is not
a plugin behaviour here — it falls out of the config declaring exactly **one**
package (`"."`, `release-type: simple`) whose single version is fanned out to
every crate:

| Mechanism | What it patches |
|---|---|
| `release-type: simple` | `version.txt` and `.release-please-manifest.json` |
| `extra-files` → `type: toml`, `jsonpath: $.workspace.package.version` | `Cargo.toml`; the 8 workspace members inherit it via `version.workspace = true` |
| `extra-files` → 5 × `type: generic` | the lines between `# x-release-please-start-version` and `# x-release-please-end` in `hyperdb-api-core`, `hyperdb-api`, `hyperdb-api-derive`, `hyperdb-mcp`, and `hyperdb-compile-check` — the inter-crate `= "X.Y.Z"` pins, plus `hyperdb-compile-check`'s own `version` (it is outside the workspace, so it cannot inherit) |

> **Do not "fix" this by adding the `linked-versions` plugin.** That plugin
> exists to synchronize versions across *multiple* release-please packages.
> This repo has one. Adding it would either be inert or force a multi-package
> layout, and would break a setup that works. If a new crate joins the tree,
> the correct change is to give it `version.workspace = true` (nothing to
> configure) or, if it must live outside the workspace, add an
> `x-release-please-start-version` marker plus an `extra-files` entry.

### Per-crate changelogs are not automated

release-please writes the **root** [`CHANGELOG.md`](../CHANGELOG.md) and
nothing else: `changelog-path` is set on the `.` package only, and no
per-crate changelog appears in `extra-files`. The nine per-crate
`CHANGELOG.md` files are entirely hand-maintained, per
[AGENTS.md](../AGENTS.md) reminder 8.

Nothing rolls their `## [Unreleased]` sections over into a dated section, so
entries for already-shipped work accumulate there indefinitely. Rolling them
over is a manual release step — see
[Rolling over the per-crate changelogs](#rolling-over-the-per-crate-changelogs).

### npm versions are stamped at publish time, not in the release PR

No `package.json` in the tree carries a `version` field or an
`optionalDependencies` block in source. Both are materialized by
[`npm-build-publish.yml`](../.github/workflows/npm-build-publish.yml) at
publish time (`npm pkg set`) from the release tag, after that workflow
verifies the tag matches the workspace `Cargo.toml` version. So a release PR
that changes no `package.json` is correct, not a bug.

### Verifying a release

Once both `release.yml` and `npm-build-publish.yml` go green:

- <https://github.com/tableau/hyper-api-rust/releases> should list the
  new tag with auto-generated release notes.
- Each crate appears on crates.io under the new version: e.g.
  <https://crates.io/crates/hyperdb-api/X.Y.Z>.
- `npm view hyperdb-mcp version` and
  `npm view hyperdb-api-node version` report the new version.

### Re-running a partial failure

The `release` workflow is mostly idempotent but there are two sharp edges:

- **crates.io is append-only.** If the workflow publishes `hyperdb-api-core
  v0.2.0` and then fails on `hyperdb-api`, you cannot republish
  `hyperdb-api-core v0.2.0` — that version is burned. The fix is to land a
  follow-up `fix:` commit on `main`, let release-please open a release PR
  for `0.2.1`, and merge that.
- **rate limits during the first publish of a brand-new crate.** crates.io
  caps "new crate" creations at one per ~10 minutes. The first time we
  publish a fresh crate name, the workflow may 429 partway through the
  `Publish in dependency order` step. Wait for the cooldown printed in the
  error and rerun via Actions → `release` → "Run workflow", entering the
  same tag name in the `tag` input. Already-published crates fail loudly
  with "already exists on" and the publish step skips that crate and
  continues with the next one.

### Re-running release.yml against an existing tag

For cases where the tag already exists in `origin` (e.g. you want to
rerun `release.yml` after a transient infra failure), use the Actions UI:

1. Actions → `release` → "Run workflow".
2. Enter the existing tag name in the `tag` input (e.g. `v0.2.0`).
3. Click Run.

The workflow's regex validator rejects malformed tag names, and
`concurrency: release` prevents racing with an in-flight run.

## Secrets

| Secret | Used by | Scope |
|---|---|---|
| `RELEASE_PLEASE_TOKEN` | [release-please.yml](../.github/workflows/release-please.yml) | Classic PAT; triggers CI on release-please PRs (see below) |
| `CARGO_REGISTRY_TOKEN` | [release.yml](../.github/workflows/release.yml) `publish` job | `cargo publish` to crates.io |
| `NPM_TOKEN` | [npm-build-publish.yml](../.github/workflows/npm-build-publish.yml) `publish-npm` job | `npm publish` to npmjs.org |
| `GITHUB_TOKEN` | Every workflow | Auto-provided by GitHub Actions; used to post releases, download artifacts, verify CI status |

### Why release-please needs a PAT

GitHub Actions suppresses workflow triggers on events created by
`GITHUB_TOKEN` (anti-recursion protection). Without a PAT, PRs opened
by release-please don't trigger CI. The workaround is a PAT
stored as `RELEASE_PLEASE_TOKEN`.

### Option A: Classic PAT (current setup)

The token was originally issued as a **fine-grained** PAT, but that
approach didn't work in practice — the `tableau` org restricts
fine-grained PAT access (org admin approval required per-token via
<https://github.com/organizations/tableau/settings/personal-access-tokens>),
and it was never approved there. A **classic** PAT sidesteps that
approval flow entirely (it only needs org SSO authorization, which is
typically already granted), so that's what's actually deployed.

1. Go to <https://github.com/settings/tokens> (classic tokens, on
   your personal GitHub account — not an EMU/org-managed account) →
   **Generate new token → Generate new token (classic)**.
2. Configure:
   - **Note:** `release-please-hyper-api-rust`
   - **Expiration:** currently set to **no expiration** (see below)
   - **Scopes:** `repo` (full) and `workflow` — classic tokens don't
     have fine-grained contents/PR permissions, so `repo` covers both
3. Click "Generate token" and copy it.
4. If the token page shows an **"Enable SSO" / "Configure SSO"**
   dropdown next to it, authorize it for the `tableau` org — without
   this the secret update succeeds but the token can't touch the repo.
5. Add it as a repo secret:

   ```bash
   gh secret set RELEASE_PLEASE_TOKEN --repo tableau/hyper-api-rust
   ```

6. The [release-please workflow](../.github/workflows/release-please.yml)
   references this secret via `token: ${{ secrets.RELEASE_PLEASE_TOKEN }}`.

**Expiration:** as of the 2026-08-24 rotation, the token is set to
**no expiration**, specifically to stop the recurring 401-on-expiry
failure mode described below. This trades away forced rotation for
reliability — if that tradeoff changes (e.g. a security review flags
non-expiring PATs), switch to Option B (GitHub App token) instead of
going back to a time-boxed classic PAT.

**Rotation (if it's ever time-boxed again):** generate a new one with
the same settings and update the secret. Release-please will fail with
a 401 until the secret is refreshed — CI on `main` pushes will still
show the failure clearly.

### Option B: GitHub App token (recommended for larger teams)

A GitHub App token isn't tied to any individual's account and never
expires (tokens are minted per-run). Preferred for org-owned repos or
when multiple maintainers need the pipeline to work independently.

1. **Create a GitHub App** (org-level: Settings → Developer settings →
   GitHub Apps → New):
   - **Name:** `hyper-api-rust-release-please`
   - **Permissions → Repository:**
     - Contents: Read and write
     - Pull requests: Read and write
   - No webhook URL needed (uncheck "Active" under Webhook)
   - Generate a private key and download it
2. **Install the App** on `tableau/hyper-api-rust` (or all repos in the
   org if you want it shared).
3. **Store credentials** as repo secrets:

   ```bash
   gh secret set APP_ID --repo tableau/hyper-api-rust        # numeric App ID
   gh secret set APP_PRIVATE_KEY --repo tableau/hyper-api-rust  # PEM file contents
   ```

4. **Update the workflow** to mint a short-lived token each run:

   ```yaml
   jobs:
     release-please:
       runs-on: ubuntu-24.04
       steps:
         - uses: actions/create-github-app-token@v2
           id: app-token
           with:
             app-id: ${{ secrets.APP_ID }}
             private-key: ${{ secrets.APP_PRIVATE_KEY }}
         - uses: googleapis/release-please-action@v5
           with:
             config-file: release-please-config.json
             manifest-file: .release-please-manifest.json
             token: ${{ steps.app-token.outputs.token }}
   ```

This mints a token scoped to the installation that expires in 1 hour —
no rotation needed, no personal account dependency.

## Issue & PR templates

There are no `.github/ISSUE_TEMPLATE/` or `.github/pull_request_template.md`
files today; Issues and PRs use GitHub defaults. Contributors still
follow the [Contribution Checklist](../CONTRIBUTING.md#contribution-checklist)
manually.

## Branch protection

Branch protection rules on `main` are configured via GitHub's repo
settings (not in this repo as config-as-code). The expected invariants:

- All PRs require at least one approval.
- `ci` must pass before merge.
- `version-forward` should be a required status check. That is the *job* name in
  [`verify-release-pr-version.yml`](../.github/workflows/verify-release-pr-version.yml),
  and GitHub's required-check picker lists checks by their check-run (job) name —
  so searching for the workflow **file** name (`verify-release-pr-version`) finds
  nothing; search for `version-forward`. The job always runs and reports `success`
  on non-release PRs (only `release-please--branches--*` branches are actually
  compared), so requiring it never blocks an ordinary PR — it gates only a release
  PR that regressed the version (#308). Until it is marked required it is advisory:
  visible on the PR but non-blocking.
- Force-push and deletion are blocked.
- Tags matching `v*.*.*` can only be pushed by maintainers (enforced via
  tag protection rules, separate from branch protection).

**Planned — migrate `main` to a ruleset after 1.0.0.** These invariants are
enforced today via GitHub's *classic* branch protection.
[Rulesets](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/about-rulesets)
are the successor: layerable (multiple can apply to one branch and the enforced
requirements are their union), readable by anyone with repo access (classic
rules are admin-only), and able to govern branches **and** tags — the `v*.*.*`
rule above — in one place. We intend to convert `main` to a ruleset once the
1.0.0 release has shipped, not during the release-candidate cycle, so
branch-gating mechanics don't change mid-release. Classic rules and rulesets
coexist and the most restrictive combination wins, so the switch can be staged
and verified without a protection gap. After converting, update this section to
point at **Settings → Rules → Rulesets** instead of **Settings → Branches**.

Check the actual live settings under
**Settings → Branches** and **Settings → Tags** on the GitHub UI.

## When something breaks

- **CI failures on `main`:** investigate and fix forward. The cancel-on-new-push
  concurrency only applies to PRs; main-branch runs always complete, so a
  broken main is a real signal.
- **`release` workflow failure during `verify`:** the tag already exists,
  so manual re-tagging isn't needed. Land the fix on `main`, then re-run
  `release.yml` against the same tag from the Actions UI (`Run workflow`
  → enter tag → run). Or, if the fix changes the tag contents, let the
  next release-please PR mint a fresh patch tag.
- **`release` workflow failure during `publish`:** the already-published
  crates are burned (crates.io is append-only). Land a `fix:` commit on
  `main` and merge the next release-please PR. Don't try to retag the
  partially-published version.
- **`verify-hyperd-pin` failure:** the pinned hyperd release URL 404'd.
  Check the Tableau releases page, update
  [`hyperdb-bootstrap/hyperd-version.toml`](../hyperdb-bootstrap/hyperd-version.toml)
  with the new version + fresh SHA-256s, and open a PR.
- **Newly-flagged `cargo-audit` advisory on `main`:** open a PR with a
  dep bump (or, if no fix is yet available, document the waiver in
  [`deny.toml`](../deny.toml) with an expiration date).

## Related docs

- [CONTRIBUTING.md](../CONTRIBUTING.md) — governance model, PR workflow, contribution checklist.
- [docs/RUST_GUIDELINES.md](RUST_GUIDELINES.md) — coding standards enforced by `ci.yml`.
- [AGENTS.md](../AGENTS.md) — codebase architecture and build commands for contributors.
- [deny.toml](../deny.toml) — `cargo deny` policy (licenses, advisories).
- [README.md → Installing the CLIs](../README.md#installing-the-clis) — user-side install paths (npm + cargo install).
