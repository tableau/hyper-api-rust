# HyperDB MCP Agent UX and Operational Diagnostics — Design

**Status:** Proposed (architecture approved; written-spec review pending)  
**Date:** 2026-08-13  
**Author:** Stefan Steiner with Codex Desktop (ultra reasoning)  
**Base branch:** `origin/main` @ `87e0b9d`  
**Working branch:** `codex/hyperdb-mcp-agent-ux`

## Context

Firsthand use of the installed `hyperdb-mcp` exposed a connected set of
operator and agent-experience problems:

- an installed npm wrapper, native MCP binary, Rust API build, `hyperd`, and
  resident daemon can all have different identities, but the current surfaces
  do not make those identities comparable;
- a persistent-database lock can surface as a generic internal error even when
  Hyper supplies SQLSTATE `55006`;
- tools route correctly between the ephemeral primary, the reserved persistent
  database, and user attachments, but most results do not say which target was
  actually selected;
- the full tool catalog is large enough to deserve measurement and a deliberate
  disclosure policy;
- `chart` is valuable as a direct SQL-to-image diagnostic, but ranked bars and
  common diagnostic presentation controls are awkward or absent; and
- public documentation has drift around degraded status, read-only behavior,
  chart parameters, persistent terminology, and `hyperd` path resolution.

The original review is recorded in
`/Users/ssteiner/dev/ssteiner-ai/notes/hyperdb-mcp-agent-ux-review-2026-08-12.md`.
This specification converts that review into an implementation contract.

Three independent read-only investigations mapped the operational, routing,
tool-schema, chart, packaging, and compatibility surfaces before this design
was approved. They also exposed several adjacent correctness defects described
below. No implementation changes were made during that investigation.

### Verified baseline

- The live MCP catalog contains 33 tools.
- A minified live `tools/list` measurement contained 53,645 UTF-8 bytes:
  17,203 bytes of tool descriptions and 34,566 bytes of input schemas. This is
  a wire-size measurement, not a model-token count; client transformation and
  tokenizer choice affect the latter.
- `chart` is the largest individual tool entry at 6,495 bytes in that
  measurement.
- The isolated base worktree passes
  `HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp` with
  exit code 0. Eight daemon stress tests are documented as ignored by the
  existing suite.
- Hyper-backed tests require permission to bind a local callback listener. A
  sandboxed run predictably fails with `Failed to create callback listener`;
  the identical command passes when local loopback binding is allowed.

## Goals

- Make installation, binary, API, daemon, and persistent-path mismatches
  diagnosable even when MCP startup or database attachment is unhealthy.
- Turn persistent attachment contention into a structured, actionable error
  without misclassifying unrelated SQLSTATE `55006` failures.
- Make the post-resolution database target visible in every success response
  from a database-routed tool.
- Establish a reproducible tool-catalog measurement and compatibility contract
  before changing disclosure behavior.
- Retain `chart` as a bounded quick-diagnostic utility and add the few
  presentation controls needed for common analytical checks.
- Correct public contract drift and use one vocabulary for the database model.
- Preserve current defaults and existing MCP tool availability.

## Non-goals

- No new MCP `doctor` tool or diagnostics resource. The existing `status` tool
  is the in-protocol diagnostics surface; the new doctor is a native CLI
  subcommand that still works when MCP registration cannot.
- No default-core tool profile, tool removal, tool grouping, dynamic route
  activation, or read-only-specific schema filtering in this tranche.
- No change from the ephemeral `local` primary as the default target.
- No process killing, daemon takeover, discovery-file deletion, database
  opening, or OS-specific lock-owner discovery from `doctor`.
- No arbitrary PATH enumeration, shell-shim reconstruction, or unsupported
  `hyperd --version` invocation.
- No generic SQLSTATE `55006` mapping.
- No chart replacement, dashboard/layout system, faceting, stacking, symlog,
  x-axis logarithms, histogram logarithms, custom log bases, or automatic label
  collision solver.
- No release, version bump, push, pull request, merge, or publication as part of
  this implementation branch.

---

## Design

### 1. Side-effect-free `hyperdb-mcp doctor`

Add a sibling Clap subcommand:

```text
hyperdb-mcp doctor [--json]
```

The existing global database and daemon flags remain valid inputs. Doctor
returns before logging setup, directory creation, engine warm-up, daemon spawn,
or persistent-database attachment.

The default presentation is concise human-readable text. `--json` emits the
same typed report as minified or pretty JSON suitable for issue reports and
automation. Successfully collecting a report exits zero even when warnings are
present; malformed CLI arguments or an inability to serialize the report remain
ordinary command failures. A missing daemon or as-yet-uncreated default
persistent file is informational rather than automatically unhealthy.

#### Report model

The report has four stable top-level sections plus warnings:

```json
{
  "status": "ok",
  "installation": {},
  "configuration": {},
  "daemon": {},
  "tool_catalog": {},
  "warnings": []
}
```

`installation` reports:

- the actual native executable from `std::env::current_exe`;
- the full MCP build identity from `mcp_version_string()`;
- the full Rust Hyper API identity from `hyper_api_version_string()`; and
- optional launcher-reported npm metadata.

`configuration` reports:

- persistent mode (`persistent_attached` or `ephemeral_only`);
- resolved persistent path and its source (`cli`, deprecated CLI alias,
  environment, platform default, or disabled);
- whether the resolved file and parent currently exist;
- daemon state directory, discovery path, and client-log path;
- the observed `HYPERD_PATH`, whether it names an existing file/directory, and
  the upward-search `.hyperd/current/hyperd` candidate when applicable; and
- the effective read-only and no-daemon flags.

Doctor reports the actual resolution facts available without starting Hyper. It
does not claim that arbitrary PATH lookup occurs: current runtime resolution
uses `HYPERD_PATH` or an upward `.hyperd/current/hyperd` search.

`daemon` reports one of these discovery states:

- `missing`;
- `malformed`;
- `parsed_unreachable`;
- `live_from_discovery`; or
- `live_from_scan`.

When available, it includes PID, endpoint, health port, start time, plain semver
used by takeover logic, full build identity, and native executable path. A
non-mutating raw discovery read and identified PING/scan supply these facts.
Doctor must not call the current cleanup-oriented `discover()` path, because
that path deletes a stale discovery file.

`tool_catalog` reports the full-profile tool count and minified serialized byte
size from the same generated router contract used by MCP. Initialization
instructions and `get_readme` sizes are reported separately rather than folded
into the tool-schema number.

#### npm launcher identity

The Node wrapper already resolves both its umbrella package and the selected
platform package before spawning the native binary. It will pass one private
JSON environment value containing only:

- wrapper package name, version, and package path;
- selected platform package name, version, and package path; and
- selected native executable path.

Rust treats these values as launcher-reported metadata, validates the expected
shape, and never blindly re-emits unknown keys. Source manifests intentionally
lack versions, so unavailable local-development values remain `null` rather
than being guessed. Direct Cargo or crates.io launches report launcher metadata
as absent.

Warnings identify mismatched wrapper/platform/native base versions, malformed
launcher metadata, a stale or malformed daemon record, and a live daemon whose
build/executable differs from the current client. They describe evidence, not a
guessed lock owner.

#### Compatibility-safe daemon record

The current public `DaemonInfo` Rust struct remains unchanged so downstream
exhaustive struct literals do not break. A version-tolerant internal discovery
record wraps or flattens the existing fields and adds optional build identity
and executable path. Existing public read/write helpers retain their behavior;
new internal helpers expose the richer record to doctor and daemon status.

Old discovery JSON must parse, new readers must accept absent identity fields,
and takeover comparison must continue using the existing plain semver only.
Unknown fields remain forward compatible.

### 2. Shared installation identity and status contract

The pure installation-identity collector is shared by CLI doctor and the MCP
`status` response. Filesystem/daemon probes remain doctor-specific where doing
them on every status call would be unnecessary.

Both full and degraded status responses gain:

- root `mcp_version`;
- the correctly labeled existing `hyper_rust_api_version`;
- an additive `installation` object; and
- `default_database: "local"`.

Existing fields are preserved. In particular, full engine statistics and the
current `engine` block retain their names.

The existing degraded response bug is fixed: it currently writes the MCP build
string into `hyper_rust_api_version`. Full and degraded paths must pass the same
version-identity assertions.

The documented degraded contract becomes:

- `engine_busy: false` means the complete status path ran;
- `engine_busy: true` means a prompt partial observer response was returned and
  the caller should retry for SQL-dependent statistics;
- omission of table counts, row totals, disk usage, ephemeral path, and log
  details is intentional while degraded; and
- `hyperd_running: false` is not definitive in a degraded local response or
  when daemon discovery is unavailable.

The stale `hyper://readme` status consumer is corrected to use the keys actually
emitted by `Engine::status`. Broader unification of resource and tool response
shapes is deferred.

### 3. Adjacent daemon reliability fixes

Two confirmed defects are included because they directly undermine the new
diagnostic story:

1. `hyperdb-mcp daemon status --port <N>` currently discards `<N>`. Status must
   probe the explicit port when supplied and use discovery plus scan only when
   it is absent.
2. A client reporting a dead `hyperd` currently targets the configured base
   port rather than the health port of the daemon it actually discovered. The
   report path must use the cached discovered health port, matching heartbeat
   routing.

No new `hyperd` flags or takeover semantics are introduced.

### 4. Contextual persistent-lock classification

Persistent attachment currently wraps every failure as `INTERNAL_ERROR`, which
loses the existing `RESOURCE_BUSY` classification. Add a context-specific
conversion used only by default persistent attachment.

When that attach operation returns SQLSTATE `55006`, or a legacy message already
recognized as a busy resource, the MCP error is:

- code: `RESOURCE_BUSY`;
- message: the effective persistent path plus the preserved raw Hyper message
  and SQLSTATE;
- suggestion: run `hyperdb-mcp doctor`, compare client/daemon identities, close
  the actual Hyper/Tableau process holding the file or choose another
  persistent file, and retry.

The wording lists possible owners without asserting which one holds the lock.

This classification must not be added to the global SQLSTATE mapper. Hyper also
uses `55006` for an unreadable Windows `COPY FROM` source, which is not database
contention. A regression test holds that boundary.

Warm-up continues to leave the MCP server available after initialization
failure; the first relevant tool call receives the structured error, while
`status` and CLI doctor remain diagnostic paths.

### 5. Canonical resolved-database metadata

Every success response from a tool that accepts database routing gains:

```json
"resolved_database": "local"
```

Values are the result after precedence and canonicalization:

- omitted target, explicit case-insensitive `local`, or `database: "local"`
  winning over `persist: true` becomes `local`;
- `persist: true` or case-insensitive `database: "persistent"` becomes
  `persistent`; and
- a user attachment becomes its lowercased canonical alias.

The field is additive and reflects the effective target, not the request echo.
A response-layer helper injects it into top-level JSON results without changing
generic engine telemetry structs.

The inventory covered by this contract is:

- `load_data`, `load_file`, and `load_files`;
- `query`, `execute`, `sample`, `describe`, and `chart`;
- `watch_directory`, `export`, and `set_table_metadata`;
- `kv_get`, `kv_set`, `kv_set_many`, `kv_delete`, `kv_list`,
  `kv_list_stores`, `kv_size`, `kv_pop`, and `kv_clear`; and
- `copy_query`, which retains `target_database` and also gains the common field.

`query` and `chart` have custom content assembly, so their existing text/image
ordering remains intact while their JSON metadata gains the field. Normal
structured/text JSON responses continue mirroring the same object for old and
new clients.

Primary-only tools such as `query_data`, `query_file`, and `load_iceberg` do not
gain a synthetic routing field in this tranche. Attachment-management results
already identify their alias/source and are not database-selection responses.

### 6. Tool-catalog measurement before disclosure changes

Add an MCP-level contract test that obtains the generated catalog through
`tools/list` and asserts:

- exactly the existing 33 sorted tool names;
- no MCP `doctor` tool;
- minified total and per-tool byte accounting;
- separate name, description, input-schema, and other-field byte totals;
- absence or presence of output schemas and annotations explicitly; and
- separately measured initialization instructions and `get_readme` payload.

The test uses a reviewed high-water byte budget rather than a brittle exact
serialization equality. The initial budget is chosen after measuring this
branch's unchanged base router and must leave only modest deliberate headroom
for the new chart fields and corrected status description. The report is
visible under `--nocapture` and included in the final verification evidence.

This work does not infer a token count from bytes. Any later core-profile
proposal must measure the exact transformed tool payload with the target
client, model tokenizer, and versions, and must compare cold start with and
without initialization instructions and `get_readme`.

Core/full profiles are deferred because:

- current core membership has no usage evidence;
- the stored router is not currently used by the default generated handler, so
  profile filtering requires deliberate router rewiring;
- hidden tools would require client configuration or reliable dynamic
  `tools/list_changed` support; and
- resources/prompts are primary-only and cannot yet replace routed query and
  describe tools for persistent or attached databases.

### 7. Keep `chart` as a bounded diagnostic

`chart` remains a direct SQL-to-inline-image convenience, not a general
visualization framework. Removing Plotters would simplify some native
dependencies but would not simplify the npm wrapper or release matrix, and no
clean A/B package-size or build-time measurement currently supports removal.

The MCP input gains four optional controls:

```text
bar_orientation: "vertical" | "horizontal"   (default "vertical")
y_scale:         "linear" | "log"             (default "linear")
show_legend:     boolean                        (default true)
label_values:    boolean                        (default false; bars only)
```

Defaults preserve existing output behavior. Parameter semantics are based on
data roles rather than physical screen axes: `x` remains the bar category,
`y` remains the numeric measure, and `y_range`/`y_scale` control that measure
even when horizontal bars draw it along the physical x-axis.

#### Horizontal bars

- Preserve first-seen SQL row order and draw the first ranked row at the top.
- Preserve grouped multi-series behavior and existing color mapping.
- Swap axis descriptions appropriately and reserve more category-label space.
- Document that callers should increase chart height for long rankings rather
  than adding speculative automatic sizing.

#### Legend and value labels

- `show_legend: false` suppresses legends for bars, lines, and scatter plots.
- Existing `label_points: true` continues to suppress the line/scatter legend
  regardless of `show_legend`.
- `label_values: true` labels bar values only, using the original scalar display
  form. Using it with another chart type is a caller-facing invalid argument.
- No collision-avoidance or formatting language is added.

#### Positive logarithmic measure scale

- `y_scale: "log"` is supported for bar, line, and scatter charts only.
- Every plotted measure and both explicit `y_range` endpoints must be finite
  and strictly positive.
- Zero, negative, mixed-sign, non-increasing, or non-finite ranges return a
  caller-facing invalid argument; values are never silently dropped or
  clamped.
- Logarithmic bars begin at the effective positive lower bound rather than zero.
- A single repeated positive value receives a sensible multiplicative padded
  range.
- Histogram log behavior, x-log, symlog, negative-only log, and custom bases are
  deferred.

#### Existing correctness fixes included

- Bars always treat `x` as categorical. The current `x_as_category: false`
  route can create numeric positions against a category-count axis and put bars
  off-canvas.
- `y_range` is applied to bars; it is currently documented but ignored.
- Explicit ranges are validated as finite and strictly increasing before
  Plotters receives them.
- Existing temporal line/scatter documentation and test names are corrected to
  match proportional temporal rendering.

#### Preserve the published Rust surface

`ChartOptions` is public and can be constructed with exhaustive struct literals.
Adding fields would be a source-breaking Rust change even though the MCP schema
change is additive. Keep `ChartOptions` and the public `render_chart` entrypoint
source-compatible. Introduce an internal presentation-options type and an
internal extended renderer; the public function delegates with presentation
defaults, while the MCP handler uses the extended path.

### 8. Documentation and terminology contract

Use these terms consistently:

- **local** — the ephemeral primary database, discarded with the session;
- **persistent** — the reserved durable database attached under the
  `persistent` alias; and
- **attached database** — a user-supplied alias added by `attach_database`.

`workspace` remains only where compatibility requires it: the deprecated
`--workspace` alias, existing resource URI, or historical release text.
Internal identifier renaming is not required when it would create unrelated
churn.

Update all affected public surfaces:

- CLI help for doctor, daemon status, read-only mode, and `hyperd` resolution;
- the `status` and `chart` tool descriptions;
- `hyperdb-mcp/src/readme.rs`;
- `hyperdb-mcp/README.md`;
- `hyperdb-mcp/SMOKE_TESTS.md`;
- chart demo comments and option examples;
- resource README rendering and semantic tests; and
- `hyperdb-mcp/CHANGELOG.md` under `## [Unreleased]`.

Correct the current read-only drift: documentation must match every actual
write guard, and `unwatch_directory` and Hyper-format export remain allowed.
Do not hand-edit workspace/package versions or the root generated changelog.

---

## Compatibility and versioning

- The default router remains the full 33-tool surface.
- No existing MCP tool, parameter, result field, resource URI, prompt, CLI flag,
  or constructor is removed or renamed.
- New result fields and MCP chart parameters are additive.
- Existing chart defaults remain unchanged.
- `ChartOptions` and public `DaemonInfo` remain source-compatible by keeping new
  presentation and discovery metadata in separate internal types.
- Old daemon discovery files remain readable.
- Plain semver remains the only daemon takeover comparison key.
- The database default remains `local`.
- Public changes receive per-crate `## [Unreleased]` entries and Conventional
  Commits. Release automation, not this branch, owns version changes.

## Error handling

- Doctor distinguishes absent, malformed, unreachable, and live state without
  mutating it.
- Invalid launcher JSON becomes an explicit warning; it cannot crash startup.
- Installation paths that cannot be represented as UTF-8 use a lossless or
  clearly marked display representation rather than panicking.
- Chart option/range/log violations return `INVALID_ARGUMENT` before rendering.
- Persistent attach contention preserves the original Hyper error and SQLSTATE
  inside a contextual `RESOURCE_BUSY` response.
- Non-attach SQLSTATE `55006` behavior is unchanged.
- Existing errors and structured/text response mirroring remain compatible.

## Testing strategy

All behavior is developed red-before-green. A test must fail for the expected
reason before production code is written, and the implementing agent records
both the red and green commands/output.

### Doctor and installation identity

- Native doctor under isolated environment variables reports native/API/path
  facts and absent npm metadata honestly.
- Doctor creates no state directory, discovery file, persistent file, log,
  daemon, or scratch database.
- Human and JSON reports carry the same facts.
- Valid npm metadata appears; deliberately mismatched versions warn; malformed
  metadata warns without crashing.
- Old and enriched daemon discovery JSON both parse.
- Stale/malformed discovery remains on disk after doctor.
- A daemon found by explicit discovery and one found only by scan are
  distinguished.
- `daemon status --port` probes the supplied port.
- restart reports use the discovered health port.

### Persistent lock

- A synthetic structured attach error with SQLSTATE `55006` becomes
  `RESOURCE_BUSY` and includes path, SQLSTATE, and doctor guidance.
- A real two-private-engine reproduction holds one persistent file open and
  verifies the second attachment returns the same actionable classification.
- A `55006` error outside persistent attach remains outside this mapping.
- The test does not invent or depend on unsupported `hyperd` flags.

### Status

- Full MCP status returns `engine_busy: false`, full statistics, and correct MCP
  and API identities.
- Holding the engine lock makes MCP status promptly return the documented
  degraded shape with `engine_busy: true`.
- Degraded output omits documented expensive fields and carries correct version
  identities.
- Tool, concise README, public README, and smoke documentation all explain the
  non-definitive degraded `hyperd_running` case.
- Workspace/readme resources consume actual status keys.

### Resolved database

- Every routed tool named in section 5 is exercised for default local routing.
- Representative tools cover `persist: true`, mixed-case `persistent`, explicit
  `database: "local"` winning over `persist: true`, and a mixed-case attached
  alias canonicalizing to lowercase.
- A structural coverage test prevents a newly routed tool from silently
  omitting `resolved_database`.
- `copy_query.target_database` remains present and consistent.
- Query content order, chart image delivery, and old-client text JSON remain
  unchanged.

### Tool catalog

- MCP `tools/list` reports the legacy 33 names and no doctor.
- The test emits total/per-tool/breakdown byte metrics under `--nocapture` and
  enforces the reviewed high-water budget.
- Initialization instructions and `get_readme` are measured separately.
- Read-only mode does not silently change the advertised full surface.
- Generated router names drive README coverage so a tool cannot bypass the
  documentation assertion.

### Chart

- Parser/validation unit tests cover accepted/default/invalid orientation and
  scale combinations.
- SVG semantic tests cover horizontal order, grouped geometry, absent legend,
  visible values, and log ticks/values without image goldens.
- PNG smoke tests cover each new renderer path.
- Range tests cover reversed, equal, non-finite, zero, negative, mixed-sign,
  one-value, and explicit-range cases.
- Existing vertical linear output remains the default.
- MCP dispatch tests prove schema deserialization and result metadata.
- Long/Unicode categories, duplicate category-series rows, missing cells,
  multiple series, and more series than the palette are covered where behavior
  is intentionally supported or rejected.

### Required gates

Focused suites run throughout implementation. Before completion, the validator
runs the documented repository gates and records exit codes and output:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test -p hyperdb-mcp
HYPERD_PATH=/Users/ssteiner/dev/bin/hyperd cargo test --workspace --exclude hyperdb-api-node --exclude hyperdb-bootstrap
```

Hyper-backed gates run with local callback-listener permission when the sandbox
otherwise blocks loopback binding. A silent, hanging, or outputless command is
not reported as passing.

## Agent and review workflow

This is a Harness plan-driven change with role separation:

1. The main thread owns this design and the implementation plan.
2. The written plan receives two independent adversarial reviews in parallel:
   one fast/mechanical and one deep/architectural. The main thread revises it.
3. Each implementation task is owned by a developer or tester agent using the
   applicable repository role brief and explicit file ownership.
4. A separate adversarial task reviewer inspects every developer result against
   the specification and real test evidence. Important findings return to a
   developer and then a fresh re-review.
5. The integrated branch receives independent fast and deep final reviews.
   Reviewer claims are reconciled against code and captured command output;
   unsupported reviewer assertions are not accepted on authority alone.
6. The main thread performs the final merge-readiness judgment. No developer
   self-report substitutes for validator output, and no agent that implemented
   a task acts as its final reviewer or publisher.

## Implementation boundaries

Expected primary change areas:

- `hyperdb-mcp/src/main.rs`
- `hyperdb-mcp/src/diagnostics.rs` (new)
- `hyperdb-mcp/src/daemon/`
- `hyperdb-mcp/src/paths.rs`
- `hyperdb-mcp/src/version.rs`
- `hyperdb-mcp/src/error.rs`
- `hyperdb-mcp/src/engine.rs`
- `hyperdb-mcp/src/server.rs`
- `hyperdb-mcp/src/chart.rs`
- `hyperdb-mcp/npm/bin.js`
- focused/new tests under `hyperdb-mcp/tests/`
- MCP README, concise README, smoke guide, demos, and crate changelog.

Changes outside this inventory require an explicit plan revision. In
particular, do not opportunistically refactor the API crates, alter default
persistence, rewrite router architecture, prune Plotters features, or change
release automation while implementing this design.

## Risks and mitigations

- **Doctor accidentally mutates state.** Keep collection pure/non-mutating and
  assert filesystem snapshots before and after.
- **Launcher metadata is mistaken for trusted identity.** Label it as
  launcher-reported, parse only known fields, and compare it with the native
  facts instead of replacing them.
- **Daemon metadata breaks public Rust callers or old files.** Preserve
  `DaemonInfo`; use a version-tolerant internal record and old-schema tests.
- **Global `55006` mapping creates false lock diagnoses.** Limit classification
  to persistent attach and retain a non-attach regression test.
- **Additive result fields break strict ad hoc consumers.** Preserve all old
  fields/content ordering and exercise every special response builder.
- **Database metadata drifts as tools are added.** Centralize injection and add
  structural coverage over every routed handler.
- **Tool profiles are chosen by intuition.** Measure first and defer membership
  until client/usage evidence exists.
- **Chart additions break published Rust struct literals.** Keep public
  `ChartOptions` unchanged and use an internal extended renderer.
- **Log rendering silently misrepresents invalid values.** Validate all data and
  ranges before Plotters and fail clearly.
- **Chart dependency cost grows unchecked.** Record current Plotters dependency
  facts and require a future clean A/B measurement before pruning or removal.
- **Parallel agents create conflicting edits.** Assign non-overlapping ownership
  where possible, run implementation tasks sequentially when they touch
  `server.rs`, and prohibit reverting other agents' work.

## Deferred decisions

The following need evidence gathered by this work or later dogfooding:

- exact core-profile membership and whether core should ever become the default;
- dynamic tool disclosure and client support for `tools/list_changed`;
- model/client-specific token savings from profiles or description changes;
- database-aware prompts and resources;
- Plotters feature pruning, package-size reduction, or chart removal;
- richer chart formatting, layout, labels, and log variants;
- OS-specific lock-owner identification; and
- an MCP doctor tool if enhanced status proves insufficient.

## Acceptance summary

The design is complete when:

- doctor is useful even with no working engine and provably leaves state alone;
- installed wrapper/native/API/daemon identities are comparable;
- persistent lock contention is `RESOURCE_BUSY` with path and actionable next
  steps, while unrelated `55006` behavior is unchanged;
- every routed success visibly names its canonical database;
- the default MCP still advertises the same 33 tools with measured schema cost;
- chart produces compatible defaults plus correct horizontal bars, legend
  control, value labels, and positive logarithmic measure scales;
- documentation and code agree on status, routing, read-only behavior, chart,
  and `hyperd` resolution;
- focused and workspace gates, including strict Clippy, have captured green
  output; and
- independent per-task and integrated adversarial reviews have no unresolved
  Critical or Important findings.
