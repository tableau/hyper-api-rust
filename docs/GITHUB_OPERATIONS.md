# GitHub Operations

How this repo uses GitHub: what runs on every push and PR, what runs on
tag pushes, how releases become crates.io publishes and downloadable
binaries, and what maintainers do by hand vs. what the automation does.

Audience: maintainers and contributors who want to know "what happens
when I push", "how do I cut a release", or "where do the pre-built
binaries on the Releases page come from".

## Repository

- **Canonical URL:** https://github.com/tableau/hyper-api-rust
- **Default branch:** `main`
- **License:** dual MIT / Apache-2.0 (see [LICENSE-MIT.txt](../LICENSE-MIT.txt), [LICENSE-APACHE.txt](../LICENSE-APACHE.txt))
- **Governance:** see [CONTRIBUTING.md](../CONTRIBUTING.md) for the
  do-acracy / meritocracy model, PR workflow, and contribution checklist.

## Workflows

Four GitHub Actions workflows live under [`.github/workflows/`](../.github/workflows/):

| Workflow | File | Triggers | Purpose |
|---|---|---|---|
| `ci` | [ci.yml](../.github/workflows/ci.yml) | `push` to `main`, all PRs, manual | fmt, clippy, full test matrix, `cargo deny`, `cargo audit`, `cargo publish --dry-run` |
| `release-please` | [release-please.yml](../.github/workflows/release-please.yml) | `push` to `main`, manual | open/update the release PR; on merge, create the `vX.Y.Z` tag + GitHub Release |
| `release` | [release.yml](../.github/workflows/release.yml) | tag push matching `v*.*.*` / `v*.*.*-rc.*`, manual | re-run tests, publish the 6 Rust crates to crates.io (`hyperdb-api-node` is published separately to npm) |
| `npm-build-publish` | [npm-build-publish.yml](../.github/workflows/npm-build-publish.yml) | GitHub Release published, manual | build npm platform packages with bundled hyperd, publish to npm registry |
| `verify-hyperd-pin` | [verify-hyperd-pin.yml](../.github/workflows/verify-hyperd-pin.yml) | changes to `hyperdb-bootstrap/hyperd-version.toml` or its source, weekly cron, manual | `HEAD` every pinned hyperd release URL to catch Tableau yanks / typos |

### CI (`ci.yml`)

Runs on **every PR** and on **every push to `main`**. Jobs:

- `rustfmt` — `cargo fmt --all --check`.
- `clippy` — `cargo clippy --workspace --all-targets -- -D warnings` (single runner; lints are platform-independent).
- `test` — full workspace test matrix on `ubuntu-latest`, `macos-14`, `windows-latest`.
- `publish-dry-run` — `cargo publish --dry-run` for each publishable crate so a broken publish manifest is caught before a tag is cut.
- `cargo-deny` — license and advisory policy enforcement per [`deny.toml`](../deny.toml).
- `cargo-audit` — RustSec advisories, `--deny warnings`.

In-progress PR CI runs are **cancelled** when a new commit is pushed to
the PR. Main-branch runs always complete. This is set via the
`concurrency` block at the top of [ci.yml](../.github/workflows/ci.yml).

### Release (`release.yml`)

Runs on the **`release: published`** event (release-please publishes
the GitHub Release after merging the release PR) or via manual
`workflow_dispatch` with an explicit tag input (for re-runs or
emergency releases). Structure:

```
verify          ← full test suite + hyperd URL check, single-platform
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
4. `hyperdb-mcp`
5. `hyperdb-bootstrap`
6. `sea-query-hyperdb`

`hyperdb-api-node` is **not** on the crates.io list (its `Cargo.toml` has
`publish = false`) — it ships as npm `hyperdb-api-node` through napi-rs's
own pipeline, which is outside this workflow today.

**Pre-release vs. stable:** the GitHub Release is marked `prerelease: true`
automatically for tags containing `-rc.`, `-alpha.`, or `-beta.` — they
show up on the Releases page but are not flagged as "Latest release".

**Concurrency:** only one release workflow runs at a time (the `concurrency:
release` group at the top of the file); a second tag push during a
release will queue, not clobber.

### npm-publish (`npm-build-publish.yml`)

Builds and publishes npm packages for `hyperdb-mcp` and `hyperdb-api-node`
with the `hyperd` database engine bundled into each platform package. This
lets end users run `npx hyperdb-mcp` or `npm install hyperdb-api-node`
without needing Rust toolchains or manual hyperd setup.

**Triggers:**

- **GitHub Release published** — fires automatically after `release.yml`
  creates/updates a GitHub Release.
- **Manual `workflow_dispatch`** — tag/branch input is optional; leave
  empty to build from the default branch HEAD (useful for testing the
  pipeline without tagging).

**Structure:**

```
verify-ci       ← checks that CI passed for this commit (gh api commit status)
   │
   └─► build-npm (matrix × 4 platforms)
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
| `darwin-arm64` | `macos-14` | `aarch64-apple-darwin` | `macos-arm64` |
| `linux-x64-gnu` | `ubuntu-latest` | `x86_64-unknown-linux-gnu` | `linux-x86_64` |
| `win32-x64-msvc` | `windows-latest` | `x86_64-pc-windows-msvc` | `windows-x86_64` |

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

**CI gate:** The `verify-ci` job checks that the combined commit status
is `success` before building. If CI hasn't passed (e.g., someone
triggers a manual dispatch on a broken commit), the workflow aborts
immediately. Note: this does **not** prevent tagging — git tags can be
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
Maintainers don't bump versions, edit changelogs, or push tags by hand — those
steps are automated based on [Conventional Commits](https://www.conventionalcommits.org/).

### How it flows

1. Contributors land PRs into `main` with conventional-commit titles
   (`feat:`, `fix:`, `chore:`, etc. — see
   [CONTRIBUTING.md](../CONTRIBUTING.md#commit-message-format)).
2. The [release-please workflow](../.github/workflows/release-please.yml)
   runs on every push to `main`. It opens (or updates) a single
   **release PR** titled `chore(main): release X.Y.Z`. That PR contains:
   - All 7 workspace crates' versions bumped (Cargo.toml + package.json).
   - The `optionalDependencies` and inter-crate version pins updated.
   - A new dated section in each crate's `CHANGELOG.md` summarizing the
     conventional commits that landed since the last release.
   - An updated `.release-please-manifest.json`.
3. A maintainer reviews the release PR. Adjust the version manually if a
   different bump is needed (e.g., promote a `0.x.0` patch to a minor) by
   editing the PR or by tagging commits with
   [`Release-As: X.Y.Z`](https://github.com/googleapis/release-please?tab=readme-ov-file#how-can-i-fix-release-notes).
4. **Merge the release PR.** release-please then:
   - Creates a `vX.Y.Z` git tag on the merge commit.
   - Creates a GitHub Release with the auto-generated changelog.
5. **Publish workflows fire automatically.** Because release-please
   uses a PAT (`RELEASE_PLEASE_TOKEN`), the `release: published` event
   triggers both `release.yml` (crates.io) and `npm-build-publish.yml`
   (npm) automatically. The npm workflow waits for CI to pass before
   building.

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

# 4. Create the tag + GitHub Release. --target accepts the merge SHA;
#    the positional arg is the tag name. release.yml fires on the
#    resulting `release: published` event and publishes to crates.io.
gh release create vX.Y.Z \
  -R tableau/hyper-api-rust \
  --target <merge-sha> \
  --title "vX.Y.Z" \
  --notes-file /tmp/vX.Y.Z-notes.md \
  --latest    # OR --prerelease for -rc / -alpha / -beta tags

# 5. Promote the release PR's label so future release-please runs don't
#    abort with "untagged, merged release PRs outstanding".
gh pr edit <release-pr-number> -R tableau/hyper-api-rust \
  --remove-label "autorelease: pending" \
  --add-label "autorelease: tagged"
```

After the tag is created:

1. Watch [`release.yml`](https://github.com/tableau/hyper-api-rust/actions/workflows/release.yml) —
   it re-runs the verify suite on the tagged SHA, then publishes the
   crates to crates.io in dependency order.
2. Watch [`npm-build-publish.yml`](https://github.com/tableau/hyper-api-rust/actions/workflows/npm-build-publish.yml)
   in parallel.
3. Confirm the new version landed (see [Verifying a release](#verifying-a-release)).

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

### How commits drive version bumps

release-please reads the [Conventional Commits](https://www.conventionalcommits.org/)
prefix on each commit since the last release tag and picks the largest
bump implied. Mark a commit as a breaking change by either appending `!`
after the type (e.g. `feat!:`) or by adding a `BREAKING CHANGE:` footer
in the commit body.

**Important pre-1.0 caveat:** while the workspace is on a `0.x.y`
version, semver treats the entire `0.x` line as unstable, and
release-please follows suit — a breaking change bumps the **minor**
component, not the major. The major component stays at `0` until you
explicitly opt into `1.0.0`.

| Commit prefix on `main` | Bump from `0.1.0` to |
|---|---|
| `fix:`, `fix(scope):` | `0.1.1` (patch) |
| `feat:`, `feat(scope):` | `0.2.0` (minor) |
| `feat!:` / `fix!:` / `BREAKING CHANGE:` footer | `0.2.0` (still minor — no major bump while pre-1.0) |
| `chore:`, `docs:`, `refactor:`, `test:`, `style:`, `ci:`, `perf:`, `build:` | no release |
| Manual `Release-As: 1.0.0` footer | `1.0.0` (forces the major bump) |

After the workspace is on `1.x.y`, the same prefixes follow normal
semver: `feat!:` will bump `1.2.3` → `2.0.0` as expected. To stabilize
the API and cut `1.0.0`, add a `Release-As: 1.0.0` footer to a
conventional-commit on `main`:

```
feat: stabilize public API

Release-As: 1.0.0
```

### Pre-releases

For an `-rc.N` / `-alpha.N` / `-beta.N` release, add a footer to a
commit on `main`:

```
Release-As: 0.2.0-rc.1
```

release-please will produce a release PR with that exact version on the
next run. Pre-release tags flow through `release.yml` and
`npm-build-publish.yml` exactly as stable releases do; the GitHub
Release is auto-flagged as `prerelease: true`, and the npm `dist-tag` is
set to `rc` / `alpha` / `beta` instead of `latest` so `npm install
hyperdb-mcp` doesn't pull a pre-release by default.

### Lockstep versioning

All 7 workspace crates share a single version number, enforced by the
`linked-versions` plugin in
[release-please-config.json](../release-please-config.json). When any
crate's commits trigger a bump, every crate moves together. This keeps
`cargo publish`'s strict inter-crate version pins (`= "X.Y.Z"`) in sync
without manual edits.

### Verifying a release

Once both `release.yml` and `npm-build-publish.yml` go green:

- https://github.com/tableau/hyper-api-rust/releases should list the
  new tag with auto-generated release notes.
- Each crate appears on crates.io under the new version: e.g.
  https://crates.io/crates/hyperdb-api/X.Y.Z.
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
  with "already uploaded" and the run will continue past them via the
  per-crate retry below.

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
| `RELEASE_PLEASE_TOKEN` | [release-please.yml](../.github/workflows/release-please.yml) | Fine-grained PAT; triggers CI on release-please PRs/tags (see below) |
| `CARGO_REGISTRY_TOKEN` | [release.yml](../.github/workflows/release.yml) `publish` job | `cargo publish` to crates.io |
| `NPM_TOKEN` | [npm-build-publish.yml](../.github/workflows/npm-build-publish.yml) `publish-npm` job | `npm publish` to npmjs.org |
| `GITHUB_TOKEN` | Every workflow | Auto-provided by GitHub Actions; used to post releases, download artifacts, verify CI status |

### Why release-please needs a PAT

GitHub Actions suppresses workflow triggers on events created by
`GITHUB_TOKEN` (anti-recursion protection). Without a PAT, PRs opened
by release-please don't trigger CI, and tags it pushes don't trigger
`release.yml` or `npm-build-publish.yml`. The workaround is a
fine-grained PAT stored as `RELEASE_PLEASE_TOKEN`.

### Option A: Fine-grained PAT (current setup)

1. Go to **Settings → Developer settings → Personal access tokens →
   Fine-grained tokens → Generate new token**.
2. Configure:
   - **Token name:** `release-please-hyper-api-rust`
   - **Expiration:** 90 days (set a calendar reminder to rotate)
   - **Resource owner:** `tableau`
   - **Repository access:** Only select → `tableau/hyper-api-rust`
   - **Permissions → Repository:**
     - Contents: Read and write
     - Pull requests: Read and write
3. Click "Generate token" and copy it.
4. Add it as a repo secret:
   ```bash
   gh secret set RELEASE_PLEASE_TOKEN --repo tableau/hyper-api-rust
   ```
5. The [release-please workflow](../.github/workflows/release-please.yml)
   references this secret via `token: ${{ secrets.RELEASE_PLEASE_TOKEN }}`.

**Rotation:** when the PAT expires, generate a new one with the same
settings and update the secret. Release-please will fail with a 401
until the secret is refreshed — CI on `main` pushes will still show the
failure clearly.

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
       runs-on: ubuntu-latest
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
- Force-push and deletion are blocked.
- Tags matching `v*.*.*` can only be pushed by maintainers (enforced via
  tag protection rules, separate from branch protection).

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
