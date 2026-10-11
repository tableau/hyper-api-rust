# Rust Coding Guidelines

This project follows the **[Microsoft Pragmatic Rust Guidelines][msft]**. They
codify idiomatic Rust for libraries and applications — error handling, API
design, `unsafe` discipline, static verification, lint posture — and we have
adopted them wholesale with a small set of documented exceptions listed at the
bottom of this document.

[msft]: https://microsoft.github.io/rust-guidelines/

Read the upstream document in full when in doubt. This page maps each guideline
to how it is enforced **in this repository** — either by a machine check that
runs in CI, or by a human rule applied at review time.

**Upstream sync:** guidelines version **2026.6** (book generated 2026-08-19),
reviewed 2026-09-04 against the full 92-guideline export.

- Version and generation date: [About page][msft-about] (`Version: 2026.6`)
- Full text for diffing: [`agents/all.txt`][msft-all]
- Upstream changes between syncs: [Changelog][msft-changelog]

[msft-about]: https://microsoft.github.io/rust-guidelines/guidelines/index.html
[msft-all]: https://microsoft.github.io/rust-guidelines/agents/all.txt
[msft-changelog]: https://microsoft.github.io/rust-guidelines/changelog/index.html

This page cites the subset that is either machine-enforced here or has been a
live review topic; the remainder are adopted by reference.

When re-syncing: read the new version off the [About page][msft-about] (the
`all.txt` export carries only a source URL and title, so the version cannot be
recovered from it), record it above, then diff the `M-*` IDs in `all.txt`
against the ones cited here. Renames do happen — the 2026.6 review found
`M-CONCISE-NAMES` had split into `M-WEASEL-WORDS` plus `M-SHORT-NAMES`.

Two upstream conventions govern how strictly to read any individual item.
Guidelines worded **must** are expected to always hold; those worded **should**
allow more flexibility. And upstream's *Golden Rule* is that the spirit counts,
not the letter: understand what a guideline safeguards before working around
it — and equally, do not follow one where doing so would violate its own
motivation. Both principles are why the deviations below are documented with
rationale rather than simply waived.

## Machine-enforced

These are enforced by lints, formatters, and CI gates. A pull request cannot
merge while any of them fails.

| Upstream rule | Tool / lint | Where configured | How it fails |
| --- | --- | --- | --- |
| **M-STATIC-VERIFICATION** (compile-time checks) | `cargo fmt`, `cargo clippy -- -D warnings`, `cargo doc -D warnings` | [Cargo.toml `[workspace.lints]`](../Cargo.toml), [.github/workflows/ci.yml](../.github/workflows/ci.yml) | CI `fmt` / `clippy` / `doc` jobs fail on any warning |
| **M-UNSAFE** (every `unsafe` block justified) | `clippy::undocumented_unsafe_blocks = "deny"` | Cargo.toml | Build fails if any `unsafe` block or `unsafe impl` lacks a `// SAFETY: …` comment |
| **M-PANIC-IS-STOP** / **M-PANIC-ON-BUG** | `clippy::correctness` + `clippy::suspicious` = `"deny"` | Cargo.toml | Many panic-adjacent bugs caught at lint time; the remainder is human-reviewed |
| **M-LINT-OVERRIDE-EXPECT** (`#[expect]` over `#[allow]`) | `clippy::allow_attributes_without_reason = "warn"`; `#[expect]` has been available since Rust 1.81, well below our MSRV | Cargo.toml | CI `clippy` job warns on bare `#[allow]`; the convention is enforced in code review. Upstream carves out **generated code and macro output**, where `#[allow]` remains appropriate — relevant to `hyperdb-api-node` (napi) and `hyperdb-api-derive` |
| **M-PUBLIC-DEBUG** (all public types `: Debug`) | `missing_debug_implementations = "warn"` | Cargo.toml | CI `clippy` fails via `-D warnings` |
| **M-CANONICAL-DOCS** (summary + sections on `pub` items) | `missing_docs = "warn"` (published crates), `cargo doc -D warnings` | Cargo.toml, workflow `doc` job | Missing rustdoc on any published crate item fails the `doc` job |
| **Integer cast discipline** (ban on narrowing `as`; see [Integer casts](#error-handling) below) | `clippy::cast_possible_truncation`, `cast_sign_loss`, `cast_possible_wrap` all `"deny"`; `cast_lossless`, `cast_precision_loss` `"warn"` | Cargo.toml | Build fails on any narrowing integer `as` cast |
| **Supply-chain: licenses** (M-OOBE adjacent) | `cargo deny check` | [deny.toml](../deny.toml), CI `deny` job | Fails on any dependency with a non-permissive license, or any unknown registry/git source |
| **Supply-chain: advisories** | `cargo audit --deny warnings` | CI `audit` job | Fails on any unfixed RustSec advisory for a crate in the lockfile |
| **M-OOBE** (builds on Tier 1 platforms without extras) | Workspace build, RHEL `rust-toolset` job | CI `test` job across linux/macos/windows, plus `.github/workflows/rhel-compatibility.yml` | The `test` matrix fails if the workspace stops building on a Tier 1 platform; the RHEL job fails if it stops building against Red Hat's system toolchain with no `rustup`. Toolchain-floor regressions are caught by the `msrv` job instead, since RHEL's stream is rolling. Note the RHEL job is path-filtered, so a docs-only PR does not run it. See [Guideline-level deviations](#guideline-level-deviations) for the `protoc` caveat |
| **M-LATEST-EDITION** (new crates target the latest edition) | `edition` + `resolver` in the workspace manifest | [Cargo.toml `[workspace.package]`](../Cargo.toml) | Reviewer-enforced. Upstream expects at least `2024`; our virtual manifest must also carry `resolver = "3"` explicitly, since Cargo does not infer a resolver from the edition for virtual workspaces |
| **M-MSRV** (MSRV is conservatively updated) | `rust-version`, `clippy.toml` `msrv` | [Cargo.toml](../Cargo.toml), [clippy.toml](../clippy.toml), CI `msrv (1.88)` job | The `msrv` job compiles the workspace on a **pinned** 1.88 toolchain, so exceeding the floor is a hard CI failure rather than a docs claim. It also covers `hyperdb-compile-check`, which declares its own `[workspace]` and so is skipped by `--workspace`. Deliberately not the RHEL job: that installs a *rolling* `rust-toolset` currently ahead of the floor. Note upstream's rule that an MSRV bump is a **minor**, not major, release — do not mark it breaking |

Every `#[expect(lint_name, reason = "…")]` in the tree is a waiver of one of
the above. Adding a new one is a conscious opt-out; review comments should
push back on anything that does not carry a convincing reason.

## Human-review

The rules below cannot (yet) be mechanically checked. They apply at code
review; point to them when requesting changes.

### API design

- **M-WEASEL-WORDS.** Avoid weasel words (`Service`, `Manager`, `Factory`,
  `Helper`). Prefer names that describe what the type *is* or *does*. The
  crate-private `ConnectionManager` in
  [hyperdb-api/src/pool.rs](../hyperdb-api/src/pool.rs) is the one place the
  word appears; it implements `deadpool::Manager` and is not public API.
  Upstream also rules out accepting builders as parameters: where repeatable instantiation is needed, take
  `impl Fn() -> Foo` rather than a `FooBuilder`.
- **M-SHORT-NAMES.** At most two short words per identifier (`AppConfig`, not
  `GlobalApplicationConfig`); no crate or module prefix baked into the name
  (`foo::Id`, not `foo::FooId`); abbreviations preferred (`CallbackFn` over
  `CallbackFunction`). Exceptions are allowed but must be exceptional and
  motivated — see the `module_name_repetitions` entry under
  [Exceptions](#exceptions) for this repo's standing waiver.
- **M-SINGLE-ITEM-PATH.** A public item is reachable through exactly one path.
  Relevant here because `hyperdb-api` re-exports from `hyperdb-api-core`:
  those are foreign re-exports and permitted, but do not additionally surface
  a crate-internal item at both `crate::module::Item` and `crate::Item`.
- **M-ASYNC-FN.** Prefer `async fn foo()` over `fn foo() -> impl Future`
  wherever both are viable. An explicit `Future` return is justified only
  inside traits, or for hot async functions under stack-size pressure.
- **M-REGULAR-FN.** Associated functions are for construction (`Type::new`,
  `Type::from_str`). Everything else is a free function or an inherent
  method; do not namespace utilities on a type for no reason.
- **M-ESSENTIAL-FN-INHERENT.** Core behavior is an inherent method; traits
  forward to it. Do not force users to `use` a trait to call a method they
  expect to exist on the type.
- **M-INIT-BUILDER.** Four or more initialization permutations → a builder.
  No `set_foo(&mut self, …)` after construction for things that could have
  been a builder.
- **M-IMPL-ASREF / M-IMPL-IO.** Public functions that take paths, strings, or
  I/O readers accept `impl AsRef<Path>` / `impl io::Read` rather than
  concrete types. Types themselves do not carry these bounds.
- **M-SIMPLE-ABSTRACTIONS.** Keep visible type-parameter nesting shallow in
  public APIs. If a signature contains more than two nested generic
  parameters, look for a helper type.
- **M-DONT-LEAK-TYPES.** Prefer `std` types in public APIs. Third-party types
  (`bytes::Bytes`, `arrow::RecordBatch`, …) are only exposed when they
  materially improve the API over an equivalent in `std`. The 1.0 policy keeps
  the `arrow` / `chrono` / `geo-types` leaks, re-exports those crates from the
  `hyperdb-api` root so callers use the exact version we build against, and
  treats a major bump of any of them as a `hyperdb-api` major (README, "Semver
  and re-exported crates"). A default-off feature cannot decouple the cadence
  and moving existing capability behind one would itself be breaking, so none
  is planned.
- **M-FEATURES-ADDITIVE.** Any feature added must be purely additive: it must
  not disable or modify a public item, must not require another feature to be
  manually enabled, and every combination must compile. Prefer a `std`
  feature over a `no-std` one. Currently near-vacuous here (`hyperdb-api` has
  no features), but load-bearing for any feature added later —
  and note that moving *existing* always-on capability behind a default-off
  feature is a breaking change, while default-on is not.

### Error handling

- **M-APP-ERROR** / **M-ERRORS-CANONICAL-STRUCTS.** Library crates
  (`hyperdb-api`, `hyperdb-api-salesforce`, `sea-query-hyperdb`) return canonical
  error enums with `Display`, `Error`, and a public constructor per variant.
  Application crates (`hyperdb-mcp`, examples) may use `anyhow`.
- **Integer casts** (repo-specific extension; the cast clippy lints are
  `deny`-level in this workspace, see `Cargo.toml`). Choose the right tool
  for each narrowing conversion:
  - Caller can tolerate failure → `T::try_from(x).ok()?` or `.map_err(...)?`
  - Caller-validated invariant → `T::try_from(x).expect("<reason>")`
  - Always fits by type algebra → `#[expect(clippy::cast_*, reason = "<proof>")]`
  - Bit-pattern reinterpret (encode/decode pairs) → `#[expect]` with the
    word "reinterpret" in the reason.
  Never introduce a new bare `as` cast between differently-sized integer
  types. The `cast_possible_truncation` / `cast_sign_loss` /
  `cast_possible_wrap` lints will block it; the lints exist to prompt a
  choice, not to block mechanically.

### Documentation

- **M-CANONICAL-DOCS.** Every `pub` item on a published crate has: a one-line
  summary sentence, optional extended prose, and the applicable sections
  (`# Examples`, `# Errors`, `# Panics`, `# Safety`). See
  [RUST_DOCUMENTATION_STYLE.md](RUST_DOCUMENTATION_STYLE.md) for the
  repo-specific conventions.
- **M-FIRST-DOC-SENTENCE.** The first sentence is under 15 words and on a
  single line; docs.rs renders it as the type's search-result snippet.
- **M-DOCUMENTED-MAGIC.** Magic numbers (`MICROS_PER_DAY`,
  `JULIAN_DAY_EPOCH`, the ~2 GiB wire-message ceiling) are `const`s with a
  comment that explains the choice, not inline literals.

### AI-assisted development

Upstream added an **AI Guidelines** category that this page did not previously
cover. It matters disproportionately here, because most changes in this
repository are agent-authored (see [AGENTS.md](../AGENTS.md)) and the failure
modes below are specifically the ones agents produce.

- **M-DESIGN-FOR-AI.** Idiomatic APIs, thorough module and item docs, strong
  types over primitive obsession, and testable APIs. Upstream's framing: what
  makes an API easy for humans makes it easy for agents, and Rust's type system
  substitutes for an agent's lack of genuine understanding — so lean on it.
- **M-NO-META-DESIGN-DOCUMENTATION.** Crate and module documentation records
  the **end state**, never the design journey. No "why we picked X over Y"
  essays in rustdoc or crate READMEs, and specifically **no self-report tables**
  claiming which guidelines a change satisfied — upstream calls that out by
  name as an agent anti-pattern. Design rationale belongs in
  [`docs/superpowers/specs/`](superpowers/specs/), which is why that convention
  exists; a high-level "Design Principles" section in a README is still fine.
- **M-SINGLE-ITEM-PATH.** Covered under [API design](#api-design). Called out
  in the AI category because agents re-export items under old *and* new paths
  during refactors instead of committing to one structure.
- **M-TAUTOLOGICAL-TESTS.** A test must not restate a constant or mirror the
  branches of the code under test — those pass by construction and raise the
  noise floor. Assert the *property* instead (monotonicity, spacing, a
  round-trip), not the literal. Where such a test exists only to satisfy a
  mutation-testing gate, skip the mutation instead.
- **M-RUST-SHAPED.** Solve Rust problems with Rust solutions; do not import
  patterns wholesale from other languages.

### Logging

- **M-LOG-STRUCTURED.** `tracing` event macros with named fields, no
  `format!`-built strings. Sensitive data (passwords, tokens, OAuth refresh
  tokens) is redacted at the emit site, not in a formatter.

## Exceptions

Lint-level relaxations live in [the root Cargo.toml](../Cargo.toml) under
`[workspace.lints.clippy]`. Microsoft's guidelines explicitly permit
per-lint opt-outs where a pedantic lint proves noisy — the blanket policy
stays at `warn`, and this table enumerates the individual exceptions.

| Lint | Level | Reason |
| --- | --- | --- |
| `clippy::module_name_repetitions` | `allow` | Existing crate/type naming is intentional (e.g. `hyperdb_api_core::types::Numeric` → `hyperdb_api_core::types::NumericError`) and churning it does not improve readability. |
| `clippy::too_many_lines` | `allow` | Style preference, not worth the churn. Prefer reviewer judgment. |
| `clippy::doc_markdown` | `allow` | Cosmetic: backticking every type name in rustdoc is churn with low reader benefit. |
| `clippy::unreadable_literal` | `allow` | Cosmetic: digit separators on wire-format constants (`MAX_JULIAN_DAY = 5373484`) reduce grep-ability. |
| `clippy::items_after_statements` | `allow` | Stylistic: benchmarks and tests use local helpers intermixed with setup logic. |
| `clippy::match_same_arms` | `allow` | Consolidating identical arms can hide semantic grouping (e.g. SQL-type size tables). |
| `clippy::must_use_candidate` | `allow` workspace-wide, `warn` in `hyperdb-api` | The lint measures **public API** ergonomics. Measured 2026-09-05: 141 sites, of which 122 are in `hyperdb-api-core` (whose `lib.rs` states it is not a public API), 10 are internal daemon helpers in the `hyperdb-mcp` binary, and 8 are prost-generated protobuf. Exactly **one** was in the flagship public API and is now `#[must_use]`. So `hyperdb-api/src/lib.rs` carries a crate-level `#![warn(clippy::must_use_candidate)]` and the workspace default stays `allow` — scoping the lint to where it means something rather than annotating 140 internal methods. |
| `clippy::multiple_crate_versions` | `allow` | Version-line splits inside our dependencies, not choices this workspace makes. Measured 2026-09-05, 17 remaining: `base64`, `hashbrown`, `syn`, `getrandom`, `rand`, `rand_core`, `thiserror`, `thiserror-impl`, `r-efi`, `wit-bindgen`, and the RustCrypto 0.10/0.11 cluster. The `windows_*` triplet previously listed here is gone — dropped with the `quinn` stack when `aws-lc-rs` was replaced by `ring`. |

**Promoted out of this table on 2026-09-05:** `clippy::missing_errors_doc` and
`clippy::missing_panics_doc` are now `deny`. The prior note claimed a large
backlog pending a post-1.0 docs pass; a real measurement found **zero** sites
for either, so the backlog had already been closed and the note was stale.

Any **source-level** waiver is expressed as
`#[expect(lint_name, reason = "<specific reason>")]`. That attribute both
silences the lint and warns if the lint *would* no longer fire — forcing a
periodic garbage-collection of stale waivers. Never use `#[allow(...)]`
without a `reason`; `clippy::allow_attributes_without_reason` will flag it.
The one carve-out upstream grants is generated code and macro output.

### Guideline-level deviations

Two upstream guidelines are knowingly not fully satisfied. Both are recorded
here rather than silently ignored.

| Guideline | Deviation | Rationale and status |
| --- | --- | --- |
| **M-OOBE** | Two build-time tool requirements beyond `cargo` and `rustc`: `protoc` (from [`hyperdb-api-core/build.rs`](../hyperdb-api-core/build.rs) calling `tonic_prost_build::configure()`), and a C/C++ toolchain plus `fontconfig-devel` (from `hyperdb-mcp`'s `plotters` → `font-kit` → `pathfinder_simd` chart font stack). | Upstream is explicit that when tools are needed to generate Rust — it names `.proto` compilation as the example — generation should run in the publishing workflow with the resulting `.rs` vendored into the crate. Both are pre-existing and tracked as follow-up. Note `protoc` is *not* packaged for UBI at all, so the RHEL job fetches it from upstream; and the chart stack could plausibly move to plotters' pure-Rust `ab_glyph` backend, which would remove the C/C++ requirement entirely at the cost of committing a font file. Measurably improved by the aws-lc-rs → ring provider switch, which removed `cmake` and a native C++ crypto build. |
| **M-MSRV** | Upstream says the MSRV should sit "a few versions behind the most recent compiler release." Ours sits ~10 behind (1.88 against a 1.98 stable). | Deliberate: 1.88.0 is what RHEL 9.7's `rust-toolset` ships, and matching it is the whole point of the enterprise compatibility contract. Revisit when RHEL rebases its Rust Application Stream. |

### Static-verification tools not yet adopted

M-STATIC-VERIFICATION names several tools beyond the five CI gates. Current
status, so this is not rediscovered each review:

- **`cargo-udeps`** — not run. Would catch unused dependencies; worth adding.
- **`miri`** — not run. Low value here: the 19 `unsafe` blocks are all
  OS/FFI (`libc`, env vars, napi casts), which Miri cannot execute anyway.
- **`cargo-hack`** — not applicable *today*. `hyperdb-api` has no feature
  flags, and the three that exist elsewhere (`hyperdb-api-core`'s
  `salesforce-auth`, `hyperdb-api-derive`'s `compile-time`,
  `hyperdb-bootstrap`'s default-on `cli`) are few enough to reason about by
  hand. **Revisit when `hyperdb-api` gains feature flags** — that is planned
  post-1.0.0, and M-FEATURES-ADDITIVE's "any combination must work"
  requirement is precisely what `cargo-hack` exists to verify.

Note also that this workspace is *stricter* than upstream in two places:
`unsafe_op_in_unsafe_fn` is `deny` where upstream suggests `warn`, and
`clippy::correctness` / `clippy::suspicious` are `deny` where upstream
suggests `warn`.

## Further reading

- [AGENTS.md](../AGENTS.md) — repo-wide coding conventions and the
  `HYPERD_PATH` build quirk.
- [CONTRIBUTING.md](../CONTRIBUTING.md) — governance, commit conventions,
  release process.
- [RUST_DOCUMENTATION_STYLE.md](RUST_DOCUMENTATION_STYLE.md) — doc-style
  rules for rustdoc, READMEs, and `docs/`.
- [DEVELOPMENT.md](../DEVELOPMENT.md) — workspace architecture and build.
