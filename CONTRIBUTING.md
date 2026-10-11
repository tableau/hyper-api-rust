# Contributing Guide For Tableau Hyper Rust API

This page lists the operational governance model of this project, as well as the recommendations and requirements for how to best contribute to Tableau Hyper Rust API. We strive to obey these as best as possible. As always, thanks for contributing – we hope these guidelines make it easier and shed some light on our approach and processes.

## Governance Model

### Community Based

The intent and goal of open sourcing this project is to increase the contributor and user base. The governance model is one where new project leads (`admins`) will be added to the project based on their contributions and efforts, a so-called "do-acracy" or "meritocracy" similar to that used by all Apache Software Foundation projects.

## Issues, requests & ideas

Use GitHub Issues page to submit issues, enhancement requests and discuss ideas.

### Bug Reports and Fixes

- If you find a bug, please search for it in the [Issues](https://github.com/tableau/hyper-api-rust/issues), and if it isn't already tracked,
   [create a new issue](https://github.com/tableau/hyper-api-rust/issues/new). Include the crate and version, your OS, the `hyperd --version` output, a minimal reproduction, and the expected vs. actual behavior. Even if an Issue is closed, feel free to comment and add details, it will still
   be reviewed.
- Issues that have already been identified as a bug (note: able to reproduce) will be labelled `bug`.
- If you'd like to submit a fix for a bug, [send a Pull Request](#creating-a-pull-request) and mention the Issue number.
- Include tests that isolate the bug and verifies that it was fixed.

### New Features

- If you'd like to add new functionality to this project, describe the problem you want to solve in a [new Issue](https://github.com/tableau/hyper-api-rust/issues/new).
- Issues that have been identified as a feature request will be labelled `enhancement`.
- If you'd like to implement the new feature, please wait for feedback from the project
   maintainers before spending too much time writing the code. In some cases, `enhancement`s may
   not align well with the project objectives at the time.

### Tests, Documentation, Miscellaneous

- If you'd like to improve the tests, you want to make the documentation clearer, you have an
   alternative implementation of something that may have advantages over the way its currently
   done, or you have any other change, we would be happy to hear about it!
- If its a trivial change, go ahead and [send a Pull Request](#creating-a-pull-request) with the changes you have in mind.
- If not, [open an Issue](https://github.com/tableau/hyper-api-rust/issues/new) to discuss the idea first.

If you're new to our project and looking for some way to make your first contribution, look for
Issues labelled `good first contribution`.

## Code Style & Guidelines

This project follows the **[Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/)**. The repo-specific adaptation — what is machine-enforced, what is reviewer-enforced, and the list of documented exceptions — is in [docs/RUST_GUIDELINES.md](docs/RUST_GUIDELINES.md).

CI enforces the machine-checkable portion on every pull request. These map to
jobs in [`.github/workflows/ci.yml`](.github/workflows/ci.yml):

- `cargo fmt --all -- --check` — `fmt` job
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` —
  `clippy` job
- `cargo test` across Linux, macOS, and Windows — `test` job
- `cargo check` on a **pinned 1.88** toolchain — `msrv (1.88)` job; the only
  gate that compiles at the declared MSRV floor, and it also covers
  `hyperdb-compile-check`, which `--workspace` skips
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` over all 8 published crates
  — `doc` job; catches broken intra-doc links and missing docs. `make doc` runs
  exactly this locally
- `cargo deny check` — `deny` job; license, advisory, and supply-chain policy
- `cargo audit --deny warnings` — `audit` job; RustSec advisories
- Node bindings build plus smoke test — `node-bindings` job
- `cargo publish --dry-run` and workspace version consistency —
  `publish-dry-run` and `version-consistency` jobs

One further gate lives in its own workflow rather than `ci.yml`: RHEL
`rust-toolset` compatibility, the `rhel-native` job in
[`rhel-compatibility.yml`](.github/workflows/rhel-compatibility.yml), which
builds in a `ubi9/ubi` container with the distro toolchain and no rustup. It is
path-filtered, so a docs-only PR does not run it.

When a lint genuinely cannot be satisfied for a given site, suppress it with `#[expect(lint_name, reason = "<specific reason>")]` rather than bare `#[allow(...)]` — the `reason` is mandatory and `#[expect]` auto-removes itself when the lint would no longer fire. See the [Exceptions](docs/RUST_GUIDELINES.md#exceptions) section of the guidelines page for the current workspace-level waivers.

## Contribution Checklist

- [ ] Clean, simple, well styled code — conforms to [docs/RUST_GUIDELINES.md](docs/RUST_GUIDELINES.md)
- [ ] Commits should be atomic and messages must be descriptive. Related issues should be mentioned by Issue number.
- [ ] Comments
  - Module-level & function-level comments.
  - Comments on complex blocks of code or algorithms (include references to sources).
- [ ] Tests
  - The test suite, if provided, must be complete and pass
  - Increase code coverage, not versa.
  - See `hyperdb-api/tests/` for integration test patterns and `hyperdb-api/tests/common/mod.rs` for shared test helpers. Borrow inspiration from existing tests.
- [ ] Dependencies
  - Minimize number of dependencies.
  - Prefer MIT, Apache-2.0, BSD-3-Clause, ISC, and MPL-2.0 licenses (consistent with this project's dual MIT/Apache-2.0 license). Enforced by `cargo deny check`; see [deny.toml](deny.toml).
- [ ] Documentation
  - Public API items have `///` doc comments with examples
  - README.md and DEVELOPMENT.md updated if behavior changes
  - New features include an example in `hyperdb-api/examples/` when applicable
- [ ] Reviews
  - Changes must be approved via peer code review

## Signed Commits

This repo requires signed commits on `main`. Any PR whose commits are unsigned will be blocked at merge time — the GitHub Actions CI runs fine on unsigned commits, but the merge button won't enable.

Set up signing once per development machine. SSH-based signing (using the same SSH key you already use for `git push`) is the simplest path:

```bash
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519.pub     # or your existing key
git config --global commit.gpgsign true
git config --global tag.gpgsign true
```

> If you have **multiple GitHub accounts** (e.g. work + personal) on the same
> machine, swap `--global` for `--local` and run the commands from inside
> this repo's checkout. That keeps the signing key + email scoped to this
> clone instead of overriding your global identity for every other repo.

If the key is not already registered on your GitHub account, add it under **Settings → SSH and GPG keys → New SSH key** with **Key type: Signing Key**. If it's already there as an Authentication Key, you don't need to add it again — GitHub verifies SSH signatures against every key on your account regardless of the type label. Trying to add the same key twice will fail with "Key is already in use"; that's fine.

Verify with `git commit --allow-empty -m "test" && git log --show-signature -1` — you should see `Good "git" signature for <your email>`. After pushing, the commit shows a green **Verified** badge on GitHub.

If `git log --show-signature` errors with `gpg.ssh.allowedSignersFile needs to be configured`, that's a local-only verification issue — your commit is signed fine and GitHub will still show Verified. To silence the error, set up a local trust file:

```bash
mkdir -p ~/.config/git
git config --global gpg.ssh.allowedSignersFile ~/.config/git/allowed_signers
echo "$(git config user.email) $(cat ~/.ssh/id_ed25519.pub)" >> ~/.config/git/allowed_signers
```

Two gotchas to avoid:

- **Your `git config user.email` must match a verified email** on your GitHub account, or commits sign fine but display as "Unverified" on the web. Check with `git config user.email` against `Settings → Emails`.
- **Squash-merge rewrites the commit** with GitHub's own signing key, so the author's signature is replaced. This is fine ("Verified by GitHub"), but if you want author-attribution-preserving signatures, merge-commit or rebase-merge preserve them.

GPG signing is also supported — see [GitHub's signing-commits guide](https://docs.github.com/en/authentication/managing-commit-signature-verification/signing-commits) for the GPG and S/MIME paths. SSH is the recommended default for this repo.

## Creating a Pull Request

1. **Ensure the bug/feature was not already reported** by searching on GitHub under Issues. If none exists, create a new issue so that other contributors can keep track of what you are trying to add/fix and offer suggestions (or let you know if there is already an effort in progress).
2. **Fork** the repository on GitHub.
3. **Clone** the forked repo to your machine.
4. **Create** a new branch to contain your work (e.g. `git checkout -b fix-issue-11`)
5. **Commit** changes to your own branch.
6. **Push** your work back up to your fork (e.g. `git push origin fix-issue-11`)
7. **Submit** a Pull Request against the `main` branch and refer to the issue(s) you are fixing. Try not to pollute your pull request with unintended changes. Keep it simple and small.
8. **Sign** the Salesforce CLA (you will be prompted to do so when submitting the Pull Request)

> **NOTE**: Be sure to [sync your fork](https://docs.github.com/en/pull-requests/how-tos/work-with-forks/syncing-a-fork) before making a pull request.

## Contributor License Agreement ("CLA")

In order to accept your pull request, we need you to submit a CLA. You only need
to do this once to work on any of Salesforce's open source projects.

Complete your CLA here: <https://cla.salesforce.com/sign-cla>

## Commit Message Format

This project uses [Conventional Commits](https://www.conventionalcommits.org/) to automate versioning and release management. Please format your commit messages accordingly.

### Commit Message Structure

```text
<type>(<scope>): <subject>

<body>

<footer>
```

- **Type** (required): The type of change (`feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `chore`)
- **Scope** (optional): The scope of the change (e.g., component name)
- **Subject** (required): A brief description of the change
- **Body** (optional): Detailed explanation of the change
- **Footer** (optional): Issue references

### Commit Types and Version Impact

| Commit Type | Version Bump | Example |
|------------|--------------|---------|
| `feat:` | Minor (0.1.0 → 0.2.0) | `feat: add connection pooling` |
| `fix:` | Patch (0.1.0 → 0.1.1) | `fix: resolve memory leak in query execution` |
| `feat!:` | Major (0.1.0 → 1.0.0) | `feat!: remove deprecated API` |
| `docs:`, `chore:`, `ci:`, `style:`, `refactor:`, `test:` | No release | `chore: update dependencies` |

> **Tip:** Use `ci:` for workflow/infrastructure fixes — not `fix:`. Reserve
> `fix:` for changes that end-users of the crate or npm package would notice.
> A `fix(ci):` commit will trigger an unintended patch release.

### Examples

```text
feat: add support for batch query execution

fix(hyperdb-api-core): resolve type mismatch

feat!: remove deprecated ResultSet methods

ci: fix chmod step in npm-build-publish workflow

chore: update arrow dependency to 56
```

## Release Process

This repo uses [release-please](https://github.com/googleapis/release-please)
to fully automate version bumps, changelog generation, tagging, and the
crates.io / npm publish dance.

### What contributors do

**Use [Conventional Commits](https://www.conventionalcommits.org/) for every
PR title.** That's it. release-please reads the merged commits to figure out
the next version and generate the changelog automatically. Commit-message
prefixes drive the version bump per the table in
[Commit Types and Version Impact](#commit-types-and-version-impact) above.

Contributors do **not** edit the **root** [`CHANGELOG.md`](CHANGELOG.md) by
hand and do **not** bump versions in `Cargo.toml` / `package.json` manually.
Both are regenerated by release-please on every push to `main`.

The **per-crate** `CHANGELOG.md` files are different: each carries a
`## [Unreleased]` section, none is managed by release-please, and you *are*
expected to append to them for user-visible API changes. See
[AGENTS.md](AGENTS.md) reminder 8 for the policy and the full file list.

### What maintainers do

The end-to-end flow lives in
[`docs/GITHUB_OPERATIONS.md` → Cutting a release](docs/GITHUB_OPERATIONS.md#cutting-a-release).
Summary:

1. Land conventional-commit PRs into `main`.
2. release-please opens (or updates) a single PR titled
   `chore(main): release X.Y.Z` containing the version bumps and CHANGELOG
   updates.
3. Review and **merge** that PR when ready to ship. Merging does *not* tag:
   `release-please-config.json` sets `skip-github-release: true`, so the
   maintainer creates the `vX.Y.Z` tag and GitHub Release by hand. This is a
   deliberate human checkpoint — see
   [`docs/GITHUB_OPERATIONS.md`](docs/GITHUB_OPERATIONS.md#cutting-a-release)
   for the exact `gh release create` invocation, including the `--prerelease`
   flag required for `-rc.N` tags.
4. **The publish workflows fire on their own** from the `release: published`
   event that `gh release create` emits — creating the Release *is* the
   publish trigger. That same event also re-runs release-please, which
   re-anchors the next `-rc.N` on the freshly cut tag (#308).
   `gh workflow run release.yml -f tag=vX.Y.Z` is for re-running a failed
   publish against an existing tag, not part of the normal path.
5. Roll over the per-crate `CHANGELOG.md` files. release-please does not
   manage them, so nothing else will — see
   [`docs/GITHUB_OPERATIONS.md`](docs/GITHUB_OPERATIONS.md#rolling-over-the-per-crate-changelogs).

For pre-releases (`-rc.N`, `-alpha.N`, `-beta.N`), release-please increments the
rc counter on its own: `release-please-config.json` carries prerelease keys, so
any conventional commit on `main` produces the next `-rc.N` with no footer. A
`Release-As:` footer remains available to pin one specific version, and when
used it has to sit in the squash commit body. Those keys must be removed when
the final release ships, or the release after it computes another rc. See
[`docs/GITHUB_OPERATIONS.md`](docs/GITHUB_OPERATIONS.md#pre-releases).

### Published Crates

| Package | Registry | Notes |
|---------|----------|-------|
| `hyperdb-api` | crates.io | Flagship public API |
| `hyperdb-api-core` | crates.io | Internal implementation detail. Published because Cargo requires it; not a stable API — depend on `hyperdb-api` instead. |
| `hyperdb-api-derive` | crates.io | Derive macros (`derive(Table)`, `query_as!`). Added directly by users rather than re-exported, to break the `hyperdb-api` → derive → `hyperdb-compile-check` cycle. |
| `hyperdb-api-salesforce` | crates.io | Salesforce Data Cloud OAuth |
| `sea-query-hyperdb` | crates.io | HyperDB dialect for sea-query |
| `hyperdb-mcp` | crates.io | MCP server CLI |
| `hyperdb-bootstrap` | crates.io | `hyperd` download helper |
| `hyperdb-compile-check` | crates.io | Compile-time SQL validation backend. Not a workspace member (it declares its own `[workspace]` to break the dependency cycle), but release-please manages its version and it must be published for `hyperdb-api-derive`'s off-by-default `compile-time` feature to resolve. |
| `hyperdb-api-node` | npm | Node.js/TypeScript bindings. `publish = false` for crates.io — the only crate in the tree that is not a Cargo publish target. |

## Code of Conduct

Please follow our [Code of Conduct](CODE_OF_CONDUCT.md).

## License

By contributing your code, you agree to license your contribution under the terms of our project [MIT](LICENSE-MIT.txt) and [Apache-2.0](LICENSE-APACHE.txt) dual license, and to sign the [Salesforce CLA](https://cla.salesforce.com/sign-cla).
