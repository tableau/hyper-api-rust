/**
 * plan-to-release — a reusable Harness driver that takes a *vetted* implementation
 * plan and drives it from adversarial review through release-ready, the manual
 * release, and a post-publish npm smoke test.
 *
 * This is the offline, operator-gated, role-separated ("Harness") workflow:
 * the main thread is the command center, every fan-out is an explicit phase,
 * and read-only validators gate the merge (doer != validator != merger).
 *
 * Invoke by name, e.g.:
 *   Workflow({ name: 'plan-to-release', args: {
 *     planPath: 'docs/superpowers/plans/2026-07-11-kv-mcp-llm-ergonomics.md',
 *     specPath: 'docs/superpowers/specs/2026-07-10-kv-mcp-llm-ergonomics-design.md',
 *     issue: 192,
 *     branch: 'feat/kv-mcp-llm-ergonomics-192',
 *     mode: 'full',            // vet | execute | sweep | release | smoke | full
 *     releaseTag: 'v0.7.0',    // required for mode release/smoke
 *     releaseStage: 'pr',      // for mode release: 'pr' | 'publish'
 *     npmPackage: 'hyperdb-mcp'
 *   }})
 *
 * MODES
 *  - vet     : adversarial plan review (2 reviewer lenses) + independent verify.
 *              Returns { blocking, findings }. If blocking, DO NOT execute — revise the plan.
 *  - execute : parse plan -> sequential iterations. Each: engineer implements,
 *              real build/test/clippy/fmt gate (captured output), adversarial review, commit.
 *  - sweep   : full E2E verification of the integrated branch + final adversarial
 *              sweep (both reviewer lenses) -> confidence verdict.
 *  - release : mechanical release automation with a 2-try check valve.
 *              releaseStage 'pr'      -> push fork branch, open/refresh upstream PR, wait CI green.
 *              releaseStage 'publish' -> merge release-please PR, HAND-CREATE the vX.Y.Z tag+release
 *                                        (repo sets skip-github-release: true), trigger both publish
 *                                        workflows, wait them green. Irreversible steps are reported,
 *                                        not silently forced.
 *  - smoke   : force-pull the freshly published npm package and exercise the new KV features
 *              through a CLEAN-ROOM `npx <pkg>@<ver> --ephemeral-only --no-daemon` server,
 *              gated on the server's self-reported version. The session's connected MCP may be
 *              config-pinned to an OLDER version, so it is NOT trusted as the source of truth.
 *  - full    : vet -> (gate) -> execute -> (gate) -> sweep. STOPS at release-ready.
 *              release + smoke are invoked separately (human-gated + post-publish timing).
 */

export const meta = {
  name: 'plan-to-release',
  description: 'Drive a vetted implementation plan through adversarial review, execution, E2E sweep, release, and npm smoke test',
  whenToUse: 'When a vetted plan file exists and you want an operator-gated, role-separated agent team to drive it to a release-ready branch (and optionally through the manual release + npm smoke test).',
  phases: [
    { title: 'Vet', detail: 'Two reviewer lenses adversarially review the plan; each finding is independently verified' },
    { title: 'Execute', detail: 'Parse plan; per iteration: engineer implements, build/test/clippy/fmt gate, adversarial review, commit' },
    { title: 'Sweep', detail: 'Full E2E verification + final adversarial sweep across both reviewer lenses' },
    { title: 'Release', detail: 'Push fork branch, open upstream PR, wait CI green; publish stage tags + triggers publish workflows (2-try check valve)' },
    { title: 'Smoke', detail: 'Clean-room npx spawn of the published package (--ephemeral-only --no-daemon), version-gated, exercising new KV features — session MCP not trusted' },
  ],
}

// ---------------------------------------------------------------------------
// Config (from args, with defaults tuned to this repo/workstation)
// ---------------------------------------------------------------------------
const REPO = (args && args.repo) || '/Users/ssteiner/dev/hyper-api-rust'
const planPath = args && args.planPath
const specPath = (args && args.specPath) || null
const issue = (args && args.issue) || null
const branch = (args && args.branch) || null
const mode = (args && args.mode) || 'full'
const releaseTag = (args && args.releaseTag) || null
const releaseStage = (args && args.releaseStage) || 'pr'
const npmPackage = (args && args.npmPackage) || 'hyperdb-mcp'
const upstream = (args && args.upstream) || 'tableau/hyper-api-rust'
const forkOwner = (args && args.forkOwner) || 'StefanSteiner'
const hyperdPath = (args && args.hyperdPath) || '~/dev/bin/hyperd'
// Max iterations dispatched from a parsed plan (backstop; logged if exceeded).
const MAX_ITERATIONS = (args && args.maxIterations) || 24

// Shared context every agent needs (they do NOT see this conversation).
const REPO_CTX = `You are working in the Rust workspace at ${REPO} (a fork of ${upstream}).
Non-negotiable project rules (from AGENTS.md):
- Tests start a real hyperd subprocess: ALWAYS export HYPERD_PATH=${hyperdPath} for cargo test/run.
- NEVER invent hyperd flags or engine parameters. Start servers only via HyperProcess::new() in tests or the Makefile targets.
- NEVER report a build/test as passing without seeing REAL captured output and a 0 exit code. If a command emits nothing for ~30s, treat it as HANGING/FAILED and say so.
- Match CI exactly for the lint gate: \`cargo clippy --workspace --all-targets --all-features -- -D warnings\` and \`cargo fmt --all --check\`, on the repo's pinned stable toolchain.
- BAN narrowing integer \`as\` casts (e.g. i64 as usize, usize as i64, i128 as i64). Use TryFrom; the codebase treats narrowing casts as latent data-corruption. Flag/convert any you touch.
- Propagate errors with \`?\`; never panic in library code.
- Keep sync and async twins in lockstep (KvStore <-> AsyncKvStore).
- Conventional Commits; commit with explicit \`git add <files>\`, never \`git add -A\`.
- release-please owns version numbers + the root CHANGELOG.md (x-release-please markers, extra-files). Do NOT hand-edit Cargo.toml versions, the workspace version, or root CHANGELOG.md. Per-crate CHANGELOG.md \`## [Unreleased]\` bullets ARE hand-maintained (AGENTS.md rule 8).
- GitHub: the active github.com account must be \`${forkOwner}\` (the EMU account is Unauthorized on upstream). PRs target upstream (\`${upstream}\`, which has CI runners) with \`--head ${forkOwner}:<branch>\`.`

// Harness execution constraints for any agent that polls the network or waits on
// CI. Learned the hard way on the v0.7.0 run: a single blocking \`gh ... --watch\`
// (or a \`sleep 100 && gh api ...\`) exceeds the ~2-minute Bash ceiling and is
// KILLED (Exit 143), which reads as a spurious hang/failure — not a real result.
const POLL_CTX = `
Harness polling constraints (do NOT ignore — a killed command is a false failure, not a result):
- The Bash tool has a hard ~2-minute wall-clock ceiling. A single foreground command that blocks longer is KILLED with Exit 143.
- Do NOT wait on CI with one blocking \`gh run watch --exit-status\` or \`gh pr checks --watch\` — a real CI run outlasts the ceiling and the watch gets killed mid-wait.
- Instead POLL: loop over short, non-blocking status queries (\`gh run list\`, \`gh pr checks <pr>\`, \`gh api .../check-runs\`) with a bounded sleep BETWEEN them. Keep each sleep <= 90s when the polled command is itself a network round-trip (the sleep + the API call together must stay under the ceiling); <= 100s is fine only when the polled command is a fast local check.
- Treat "no output for ~30s from a command you expected to print" as HANGING/FAILED and report it, per AGENTS.md rule 10.`

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------
const FINDINGS_SCHEMA = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          id: { type: 'string' },
          severity: { type: 'string', enum: ['critical', 'major', 'minor', 'nit'] },
          category: { type: 'string' },
          claim: { type: 'string', description: 'the exact plan/spec statement or omission at issue' },
          problem: { type: 'string', description: 'why it is wrong/risky, with source evidence (file:line) actually checked' },
          fix: { type: 'string', description: 'concrete suggested change' },
        },
        required: ['id', 'severity', 'category', 'claim', 'problem', 'fix'],
      },
    },
  },
  required: ['findings'],
}

const VERDICT_SCHEMA = {
  type: 'object',
  properties: {
    verdict: { type: 'string', enum: ['CONFIRMED', 'REJECTED', 'PARTIAL'] },
    reasoning: { type: 'string' },
    corrected_fix: { type: 'string' },
  },
  required: ['verdict', 'reasoning'],
}

const PLAN_PARSE_SCHEMA = {
  type: 'object',
  properties: {
    iterations: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          n: { type: 'integer' },
          title: { type: 'string' },
          files: { type: 'array', items: { type: 'string' } },
          acceptance: { type: 'array', items: { type: 'string' }, description: 'concrete acceptance criteria the reviewer can check' },
          commit_type: { type: 'string', description: 'conventional-commit type, e.g. feat, fix, feat!, docs, test' },
          commit_message: { type: 'string' },
        },
        required: ['n', 'title', 'files', 'acceptance', 'commit_message'],
      },
    },
    notes: { type: 'string' },
  },
  required: ['iterations'],
}

const ITERATION_RESULT_SCHEMA = {
  type: 'object',
  properties: {
    iteration: { type: 'integer' },
    implemented: { type: 'boolean' },
    gate_passed: { type: 'boolean', description: 'true ONLY if clippy + fmt + relevant tests all passed with captured 0-exit output' },
    gate_output_summary: { type: 'string', description: 'real command output excerpts proving pass/fail' },
    files_changed: { type: 'array', items: { type: 'string' } },
    committed: { type: 'boolean' },
    commit_sha: { type: 'string' },
    blockers: { type: 'array', items: { type: 'string' } },
    notes: { type: 'string' },
  },
  required: ['iteration', 'implemented', 'gate_passed', 'gate_output_summary', 'committed'],
}

const SWEEP_SCHEMA = {
  type: 'object',
  properties: {
    e2e_passed: { type: 'boolean' },
    commands_run: { type: 'array', items: { type: 'string' } },
    confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
    blocking_issues: { type: 'array', items: { type: 'string' } },
    summary: { type: 'string' },
  },
  required: ['e2e_passed', 'confidence', 'summary'],
}

const RELEASE_SCHEMA = {
  type: 'object',
  properties: {
    stage: { type: 'string' },
    success: { type: 'boolean' },
    attempts: { type: 'integer' },
    pr_url: { type: 'string' },
    ci_status: { type: 'string', description: 'success | failure | pending | unknown' },
    actions_taken: { type: 'array', items: { type: 'string' } },
    manual_commands: { type: 'array', items: { type: 'string' }, description: 'exact commands the human/main-thread must run for irreversible or blocked steps' },
    notes: { type: 'string' },
  },
  required: ['stage', 'success', 'attempts', 'notes'],
}

const SMOKE_SCHEMA = {
  type: 'object',
  properties: {
    resolved_version: { type: 'string', description: 'the version npm actually served / the fresh binary reported' },
    expected_version: { type: 'string' },
    version_match: { type: 'boolean' },
    tests: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          name: { type: 'string' },
          passed: { type: 'boolean' },
          detail: { type: 'string' },
        },
        required: ['name', 'passed', 'detail'],
      },
    },
    all_passed: { type: 'boolean' },
    summary: { type: 'string' },
  },
  required: ['resolved_version', 'tests', 'all_passed', 'summary'],
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// Adversarially review a target with two reviewer lenses, then independently
// verify every finding; keep only CONFIRMED/PARTIAL. Used for both plan-vet and
// the final sweep. `phaseName` groups agents in the progress display.
async function reviewAndVerify(targetDescription, lensBriefs, phaseName) {
  const reviews = await parallel(
    lensBriefs.map(lens => () =>
      agent(lens.prompt, { label: `review:${lens.key}`, phase: phaseName, schema: FINDINGS_SCHEMA, agentType: lens.agentType })
    )
  )
  const raw = []
  reviews.filter(Boolean).forEach((r, i) => {
    const key = lensBriefs[i].key
    ;((r && r.findings) || []).forEach(f => raw.push({ lens: key, finding: f }))
  })
  if (!raw.length) return { findings: [], blocking: false }

  const verified = await parallel(
    raw.map(item => () =>
      agent(
        `You are an independent skeptic verifying ONE review finding. Do NOT trust it — check it yourself against the real files in ${REPO}.

TARGET: ${targetDescription}

FINDING (lens: ${item.lens}):
- id: ${item.finding.id}
- severity claimed: ${item.finding.severity}
- category: ${item.finding.category}
- claim: ${item.finding.claim}
- problem: ${item.finding.problem}
- proposed fix: ${item.finding.fix}

Open the cited files/lines. Decide: CONFIRMED (real defect, fix is right), PARTIAL (real issue but proposed fix is wrong/incomplete — give corrected_fix), or REJECTED (not a real problem / misreads source). Default to REJECTED if you cannot substantiate it against actual evidence.`,
        { label: `verify:${item.lens}:${item.finding.id}`, phase: phaseName, schema: VERDICT_SCHEMA }
      ).then(v => ({ ...item, verdict: v }))
    )
  )

  const survivors = verified
    .filter(Boolean)
    .filter(r => r.verdict && (r.verdict.verdict === 'CONFIRMED' || r.verdict.verdict === 'PARTIAL'))
    .map(r => ({
      lens: r.lens,
      id: r.finding.id,
      severity: r.finding.severity,
      category: r.finding.category,
      claim: r.finding.claim,
      problem: r.finding.problem,
      fix: r.verdict.corrected_fix || r.finding.fix,
      verdict: r.verdict.verdict,
      verify_reasoning: r.verdict.reasoning,
    }))

  const blocking = survivors.some(s => s.severity === 'critical' || s.severity === 'major')
  return { findings: survivors, blocking }
}

// ---------------------------------------------------------------------------
// Phase: VET — adversarial plan review, hard gate before execution
// ---------------------------------------------------------------------------
async function vet() {
  phase('Vet')
  if (!planPath) return { blocking: true, findings: [], error: 'no planPath provided to vet' }
  const target = `The implementation plan at ${planPath}${specPath ? ` (design spec: ${specPath})` : ''}${issue ? `, for issue #${issue}` : ''}.`
  const lenses = [
    {
      key: 'mechanics',
      agentType: 'feature-dev:code-reviewer',
      prompt: `${REPO_CTX}

Adversarially review the implementation plan at ${planPath}${specPath ? ` against its design spec ${specPath}` : ''}. Find every LINE-LEVEL and MECHANICAL flaw before any code is written:
- Ambiguous or wrong file:line targets, off-by-N line ranges, stale baselines.
- Steps that contradict the current source (open the real files the plan cites and verify each claim).
- Missing edits that a later step depends on; iterations that won't compile in isolation.
- Verification commands that are wrong for this repo (e.g. forgetting HYPERD_PATH, wrong clippy scope, invented hyperd flags).
- Narrowing \`as\` casts introduced by the plan.
- Any place the plan hand-edits release-please-owned files (Cargo.toml versions, root CHANGELOG.md).
Report only real defects via the schema. Cite the file:line you checked.`,
    },
    {
      key: 'architecture',
      agentType: 'code-review',
      prompt: `${REPO_CTX}

Adversarially review the implementation plan at ${planPath}${specPath ? ` against its design spec ${specPath}` : ''}. Find ARCHITECTURAL and CROSS-FILE concerns a line-level reviewer would miss:
- Premise errors (the plan misidentifies what a symbol/function actually does).
- Missed downstream impact: does the plan account for every in-repo caller of changed APIs (search hyperdb-api, hyperdb-mcp, hyperdb-api-node, examples, tests, sea-query-hyperdb, salesforce)?
- Breaking-change correctness: is the version/release story right for release-please, and does the Node binding (hyperdb-api-node) ripple?
- Security implications (e.g. arbitrary file read via value_path), over-engineering, scope creep beyond the issue.
- Cross-file consistency: response-shape changes vs existing tests; sync/async twin drift.
Report only real defects via the schema. Cite evidence you actually checked.`,
    },
  ]
  const result = await reviewAndVerify(target, lenses, 'Vet')
  log(`Vet: ${result.findings.length} verified findings; blocking=${result.blocking}`)
  return result
}

// ---------------------------------------------------------------------------
// Phase: EXECUTE — parse plan, then drive iterations sequentially
// ---------------------------------------------------------------------------
async function execute() {
  phase('Execute')
  if (!planPath) return { gate_failed: true, error: 'no planPath provided to execute', iterations: [] }

  // Preflight: confirm we are on a feature branch, not main. (Bail loudly if main.)
  const preflight = await agent(
    `${REPO_CTX}

Run these read-only checks in ${REPO} and report findings as plain text:
1. \`git -C ${REPO} rev-parse --abbrev-ref HEAD\` — the current branch.
2. \`git -C ${REPO} status --porcelain\` — is the tree clean?
${branch ? `The expected working branch is \`${branch}\`. If HEAD is not on it, and the branch exists, note that; if HEAD is on \`main\`, that is a BLOCKER.` : ''}
Report: current branch, whether it is safe to commit here (must NOT be main), and any uncommitted changes.`,
    { label: 'execute:preflight', phase: 'Execute', agentType: 'general-purpose' }
  )
  log(`Execute preflight: ${String(preflight).slice(0, 300)}`)

  // Parse the plan into concrete iterations.
  const parsed = await agent(
    `${REPO_CTX}

Read the implementation plan at ${planPath}. Extract its ordered iterations into structured form. For each iteration capture: n (order), title, the files it touches, its concrete acceptance criteria (what a reviewer must be able to verify), the conventional-commit type (feat/fix/feat!/docs/test/...), and a one-line commit message. Preserve the plan's ordering exactly — later iterations may depend on earlier ones. Do not invent iterations that aren't in the plan.`,
    { label: 'execute:parse', phase: 'Execute', schema: PLAN_PARSE_SCHEMA, agentType: 'general-purpose' }
  )

  let iterations = (parsed && parsed.iterations) || []
  if (!iterations.length) return { gate_failed: true, error: 'plan parsed to zero iterations', iterations: [] }
  if (iterations.length > MAX_ITERATIONS) {
    log(`WARNING: plan has ${iterations.length} iterations; capping dispatch at ${MAX_ITERATIONS}. Remaining will NOT be executed this run.`)
    iterations = iterations.slice(0, MAX_ITERATIONS)
  }
  log(`Execute: ${iterations.length} iterations parsed`)

  // Sequential loop — dependent iterations sharing files must not run in parallel.
  const results = []
  for (const it of iterations) {
    const impl = await agent(
      `${REPO_CTX}

You are the ENGINEER (doer). Execute EXACTLY iteration ${it.n} ("${it.title}") from the plan at ${planPath} — read the plan for the full step detail; do not run other iterations. Touch only these files (plus their tests): ${JSON.stringify(it.files)}.

After editing, run the build/test gate and CAPTURE REAL OUTPUT:
1. \`cd ${REPO} && cargo fmt --all\` then \`cargo fmt --all --check\`
2. \`cd ${REPO} && cargo clippy --workspace --all-targets --all-features -- -D warnings\`
3. The relevant tests with the real engine, e.g. \`HYPERD_PATH=${hyperdPath} cargo test -p <crate> <filter>\` (and the async twin). If a command hangs (no output ~30s), treat it as FAILED.
Set gate_passed=true ONLY if fmt, clippy, and the relevant tests all pass with 0 exit and captured output.

If (and only if) the gate passes, commit with an explicit \`git add <files>\` (never -A) and this message: "${it.commit_message}". Report the commit sha. If the gate fails, do NOT commit; report the failure output in gate_output_summary and list blockers.

Acceptance criteria for this iteration: ${JSON.stringify(it.acceptance)}.`,
      { label: `iter${it.n}:engineer`, phase: 'Execute', schema: ITERATION_RESULT_SCHEMA, agentType: 'general-purpose' }
    )

    // If the engineer couldn't get a green gate, stop — later iterations depend on this.
    if (!impl || !impl.gate_passed) {
      results.push(impl || { iteration: it.n, implemented: false, gate_passed: false, gate_output_summary: 'engineer agent returned null', committed: false })
      log(`Execute STOPPED at iteration ${it.n}: gate not green.`)
      return { gate_failed: true, stopped_iteration: it.n, iterations: results }
    }

    // Adversarial per-iteration review (fast lens only — speed matters in the loop).
    const review = await agent(
      `${REPO_CTX}

You are the REVIEWER (validator) for iteration ${it.n} ("${it.title}"). You did NOT write this code. Review the most recent commit's diff in ${REPO} (\`git show HEAD\` / \`git diff HEAD~1 HEAD\`) against these acceptance criteria: ${JSON.stringify(it.acceptance)}.
Check for: correctness bugs, narrowing \`as\` casts, sync/async twin drift, missing error propagation, response-shape changes that break existing tests, and any deviation from the plan. Report only real defects via the schema; empty findings means the iteration passed review.`,
      { label: `iter${it.n}:review`, phase: 'Execute', schema: FINDINGS_SCHEMA, agentType: 'feature-dev:code-reviewer' }
    )
    const reviewFindings = (review && review.findings) || []
    const blocking = reviewFindings.filter(f => f.severity === 'critical' || f.severity === 'major')

    // If the reviewer flagged blocking issues, have the engineer fix them in a follow-up before advancing.
    if (blocking.length) {
      log(`Iteration ${it.n}: reviewer flagged ${blocking.length} blocking issue(s); dispatching a fix pass.`)
      const fix = await agent(
        `${REPO_CTX}

You are the ENGINEER. A reviewer flagged blocking issues on your iteration ${it.n} commit. Fix ONLY these, re-run the full gate (fmt/clippy/relevant tests, captured output), and amend or add a commit. Blocking issues:
${JSON.stringify(blocking, null, 2)}
Report the same iteration-result shape; gate_passed=true only with real green output.`,
        { label: `iter${it.n}:fix`, phase: 'Execute', schema: ITERATION_RESULT_SCHEMA, agentType: 'general-purpose' }
      )
      results.push({ ...impl, review_findings: reviewFindings, fix })
      if (!fix || !fix.gate_passed) {
        log(`Execute STOPPED at iteration ${it.n}: fix pass did not go green.`)
        return { gate_failed: true, stopped_iteration: it.n, iterations: results }
      }
    } else {
      results.push({ ...impl, review_findings: reviewFindings })
    }
    log(`Iteration ${it.n} complete: committed ${impl.commit_sha || '(see notes)'}, ${reviewFindings.length} review notes.`)
  }

  return { gate_failed: false, iterations: results }
}

// ---------------------------------------------------------------------------
// Phase: SWEEP — full E2E verification + final adversarial sweep
// ---------------------------------------------------------------------------
async function sweep() {
  phase('Sweep')

  // One heavy agent runs the integrated E2E gate (sequential; captures output).
  const e2e = await agent(
    `${REPO_CTX}

You are running the FULL end-to-end verification of the integrated branch in ${REPO}. Run and CAPTURE REAL OUTPUT (0 exit required for each), reporting exactly what passed/failed:
1. \`cd ${REPO} && cargo fmt --all --check\`
2. \`cd ${REPO} && cargo clippy --workspace --all-targets --all-features -- -D warnings\`
3. \`cd ${REPO} && HYPERD_PATH=${hyperdPath} cargo test --workspace\` (real hyperd; if it hangs with no output ~30s, treat as FAILED).
4. If examples are relevant, \`cd ${REPO} && make examples\` or run the touched examples.
Set e2e_passed=true ONLY if fmt+clippy+workspace tests all pass with captured output. Give confidence (high/medium/low) and list any blocking issues. commands_run should list what you actually ran.`,
    { label: 'sweep:e2e', phase: 'Sweep', schema: SWEEP_SCHEMA, agentType: 'general-purpose' }
  )

  // Final adversarial sweep — BOTH reviewer lenses on the integrated diff vs main.
  const target = `The integrated feature branch${branch ? ` \`${branch}\`` : ''} in ${REPO}, diffed against \`main\` (\`git diff main...HEAD\`)${issue ? `, implementing issue #${issue}` : ''}.`
  const lenses = [
    {
      key: 'final-mechanics',
      agentType: 'feature-dev:code-reviewer',
      prompt: `${REPO_CTX}

Final adversarial sweep. Review the WHOLE integrated diff \`git diff main...HEAD\` in ${REPO}. Find line-level defects that only appear on the integrated whole: narrowing casts, sync/async drift, response-shape/test mismatches, missing per-crate CHANGELOG \`## [Unreleased]\` bullets, incorrect commit types for release-please. Report real defects via the schema.`,
    },
    {
      key: 'final-architecture',
      agentType: 'code-review',
      prompt: `${REPO_CTX}

Final adversarial sweep (deep lens). Review the WHOLE integrated diff \`git diff main...HEAD\` in ${REPO}. Find cross-file inconsistencies, missed callers of changed APIs, security regressions, over-engineering, docs/README that still promise old behavior, and any breaking-change/versioning story that will confuse release-please or downstream. Report real defects via the schema.`,
    },
  ]
  const reviewResult = await reviewAndVerify(target, lenses, 'Sweep')

  const releaseReady = !!(e2e && e2e.e2e_passed) && !reviewResult.blocking
  log(`Sweep: e2e_passed=${e2e && e2e.e2e_passed}, blocking review findings=${reviewResult.blocking}, release_ready=${releaseReady}`)
  return { e2e, review: reviewResult, release_ready: releaseReady }
}

// ---------------------------------------------------------------------------
// Phase: RELEASE — mechanical automation with a 2-try check valve
// ---------------------------------------------------------------------------
async function release() {
  phase('Release')

  if (releaseStage === 'pr') {
    const r = await agent(
      `${REPO_CTX}
${POLL_CTX}

You are the PUBLISHER, releaseStage=PR. Do the mechanical, reversible release-prep steps for the feature branch${branch ? ` \`${branch}\`` : ''} in ${REPO}. Each network op gets AT MOST TWO attempts — if it fails twice, STOP that step, record it, and put the exact manual command in manual_commands. Never force irreversible actions.

Steps:
1. Ensure the active github.com account is \`${forkOwner}\`: \`gh auth switch --hostname github.com --user ${forkOwner}\` (verify with \`gh auth status\`).
2. Push the branch to the fork: \`git -C ${REPO} push -u origin ${branch || 'HEAD'}\` (2-try).
3. Open or update a PR to upstream: \`gh pr create -R ${upstream} --head ${forkOwner}:${branch || '<branch>'} --fill\` (if it already exists, fetch it with \`gh pr view -R ${upstream} --head ${forkOwner}:${branch}\`). Capture pr_url.
4. Wait for CI to FINISH by POLLING (per the Harness polling constraints above — do NOT use a single blocking \`--watch\`): loop \`gh pr checks <pr> -R ${upstream}\` with a <=90s sleep between polls until every check concludes. Report ci_status = success/failure/pending. Do NOT merge — merging to main is a human decision that triggers release-please.
Report actions_taken, pr_url, ci_status, attempts, and any manual_commands needed. success=true only if the branch is pushed, the PR is open, and CI concluded green.`,
      { label: 'release:pr', phase: 'Release', schema: RELEASE_SCHEMA, agentType: 'general-purpose' }
    )
    return { stage: 'pr', ...(r || { success: false, attempts: 0, notes: 'release:pr agent returned null' }) }
  }

  if (releaseStage === 'publish') {
    if (!releaseTag) return { stage: 'publish', success: false, attempts: 0, notes: 'releaseTag (e.g. v0.7.0) is required for the publish stage' }
    const r = await agent(
      `${REPO_CTX}
${POLL_CTX}

You are the PUBLISHER, releaseStage=PUBLISH, targeting tag ${releaseTag}. This repo uses release-please with \`skip-github-release: true\`, so the GitHub Release + git tag are created BY HAND after the release PR merges. Each network op gets AT MOST TWO attempts; if it fails twice, STOP, record it, and emit the exact manual command in manual_commands. These steps are IRREVERSIBLE once publishes fire — be conservative and verify preconditions before each.

Preconditions to VERIFY first (report and STOP if unmet):
- The feature PR for issue #${issue || '(n/a)'} is merged into \`${upstream}\` main.
- release-please has opened a \`chore(main): release ${releaseTag.replace(/^v/, '')}\` PR (\`gh pr list -R ${upstream} --search "release-please"\`).

Steps (each 2-try):
1. Confirm/merge the release-please PR (only if it is the correct version ${releaseTag}). Report its number and merge result.
2. After it merges, verify the manifest on the merge commit shows the new version:
   \`gh api repos/${upstream}/contents/.release-please-manifest.json?ref=<merge-sha> --jq '.content' | base64 -d\`.
3. HAND-CREATE the release (repo sets skip-github-release): create the tag on the merge commit and the GitHub Release:
   \`gh release create ${releaseTag} -R ${upstream} --target <merge-sha> --title ${releaseTag} --notes "<generated notes>"\` (this also creates the tag).
4. Trigger the two publish workflows (2-try each):
   \`gh workflow run release.yml -R ${upstream} -f tag=${releaseTag}\`
   \`gh workflow run npm-build-publish.yml -R ${upstream} -f tag=${releaseTag}\`
5. WAIT for both runs to FINISH successfully by POLLING (per the Harness polling constraints above — NOT one blocking \`gh run watch\`, which a real publish run outlasts and gets killed): loop \`gh run list -R ${upstream} --workflow=<name> --limit 1\` (or \`gh run view <id> --json status,conclusion\`) with a <=90s sleep between polls until both conclude. Report ci_status.
6. Promote the release PR label so future release-please runs don't abort:
   \`gh pr edit <release-pr> -R ${upstream} --remove-label "autorelease: pending" --add-label "autorelease: tagged"\`.
Report actions_taken, attempts, ci_status, and manual_commands for anything you could not complete in two tries. success=true only if the tag+release exist and both publish workflows concluded green.`,
      { label: 'release:publish', phase: 'Release', schema: RELEASE_SCHEMA, agentType: 'general-purpose' }
    )
    return { stage: 'publish', ...(r || { success: false, attempts: 0, notes: 'release:publish agent returned null' }) }
  }

  return { stage: releaseStage, success: false, attempts: 0, notes: `unknown releaseStage "${releaseStage}" (expected 'pr' or 'publish')` }
}

// ---------------------------------------------------------------------------
// Phase: SMOKE — post-publish npm smoke test via the MCP
// ---------------------------------------------------------------------------
async function smoke() {
  phase('Smoke')
  const expected = releaseTag ? releaseTag.replace(/^v/, '') : '(latest)'
  const r = await agent(
    `${REPO_CTX}

You are running a POST-RELEASE npm SMOKE TEST of the published \`${npmPackage}\` package (expected version ${expected}). The goal: prove the freshly published package pulls from npm and that the NEW KV features work end-to-end through the MCP.

CRITICAL — verify against the REAL published artifact, NOT this session's connected MCP. The connected \`mcp__hyperdb-npm__*\` server was spawned at session start and may be pinned to an OLDER version in \`~/.claude.json\` (an explicit \`${npmPackage}@X.Y.Z\`, not \`@latest\`); a live session cannot hot-swap its own MCP process. If you smoke-test through those connected tools you may exercise the OLD binary and get a FALSE GREEN — exactly the failure AGENTS.md rule 10 exists to prevent. So the clean-room \`npx\` spawn below is the PRIMARY verification path, and every assertion is gated on the server's SELF-REPORTED version.

1. Confirm npm serves the new version: \`npm view ${npmPackage} version\` and \`npm view ${npmPackage} dist-tags\`. Record resolved_version. If it is not ${expected}, the publish hasn't propagated — report version_match=false and stop early with that finding (do NOT fail the other tests spuriously).
2. Force a FRESH pull (bypass any npx cache) and prove the binary runs:
   \`npx -y ${npmPackage}@${expected} --help\` — capture output. Confirm the flags used below (\`--ephemeral-only\`, \`--no-daemon\`) actually appear in --help before relying on them (AGENTS.md rule 9 — never invent flags).
3. Drive a FRESHLY-SPAWNED clean-room server over stdio — do NOT use the session's connected \`mcp__hyperdb-npm__*\` tools as the source of truth:
   \`npx -y ${npmPackage}@${expected} --ephemeral-only --no-daemon\`
   These two flags avoid two real collisions with the session's live MCP, observed on the v0.7.0 run:
   - \`--ephemeral-only\`: the shared persistent \`workspace.hyper\` is held open by the session's server; opening it from a second process throws SQLSTATE 55006 ("database file is locked by another process"). The KV smoke checks only need the ephemeral DB, so skip persistent entirely.
   - \`--no-daemon\`: a newer client performs a daemon version-takeover on the shared port 7485 (a shipped feature) — killing the session's daemon. \`--no-daemon\` spawns a private hyperd and leaves the session's daemon alone.
   Do the MCP JSON-RPC handshake (initialize -> notifications/initialized -> tools/call). GATE FIRST: call \`status\` and assert its reported version starts with "${expected}". If it does not, STOP — you are not testing the new artifact; report version_match=false. Only if the version matches, run these checks and record each as a test:
   - kv_set returns a \`created\` field (true on first write, false on overwrite of the same key) and \`value_bytes\`.
   - kv_set with overwrite:false on an existing key returns \`{stored:false, existed:true}\` / does not clobber.
   - kv_set with value_path reads a temp file's contents (create one with a known string first).
   - kv_size returns a \`bytes\` field alongside the key count.
   - kv_set_many writes multiple entries atomically and reports created/overwritten + total_bytes.
   - kv_list with values:true returns entries with values (not just keys).
   - get_readme / the KV schema resource documents the value::json JSON-query pattern and the ::numeric scale-0 gotcha.
Record resolved_version = the server's SELF-REPORTED version (not just what npm claims), all_passed, and a concise summary. Use ONLY documented tool parameters; do not invent flags.`,
    { label: 'smoke:npm-mcp', phase: 'Smoke', schema: SMOKE_SCHEMA, agentType: 'general-purpose' }
  )
  const out = r || { resolved_version: 'unknown', tests: [], all_passed: false, summary: 'smoke agent returned null' }
  out.expected_version = expected
  out.version_match = out.resolved_version === expected
  log(`Smoke: resolved ${out.resolved_version} (expected ${expected}), all_passed=${out.all_passed}`)
  return out
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------
let result
if (mode === 'vet') {
  result = await vet()
} else if (mode === 'execute') {
  result = { execute: await execute() }
} else if (mode === 'sweep') {
  result = await sweep()
} else if (mode === 'release') {
  result = await release()
} else if (mode === 'smoke') {
  result = await smoke()
} else {
  // full: vet -> (gate) -> execute -> (gate) -> sweep. Stop at release-ready.
  const v = await vet()
  if (v.blocking) {
    log('FULL stopped at VET: blocking findings — revise the plan on the main thread before executing.')
    result = { stopped_at: 'vet', vet: v, next: 'revise plan, re-run mode=full' }
  } else {
    const e = await execute()
    if (e.gate_failed) {
      log('FULL stopped at EXECUTE: a build/test gate did not go green.')
      result = { stopped_at: 'execute', vet: v, execute: e, next: 'inspect the failing iteration on the main thread' }
    } else {
      const s = await sweep()
      result = {
        stopped_at: s.release_ready ? 'release-ready' : 'sweep',
        vet: v,
        execute: e,
        sweep: s,
        next: s.release_ready
          ? `Release-ready. Run mode=release releaseStage=pr, merge upstream, then mode=release releaseStage=publish releaseTag=${releaseTag || 'vX.Y.Z'}, then mode=smoke.`
          : 'Sweep found blocking issues or E2E failed — fix on the main thread and re-run mode=sweep.',
      }
    }
  }
}

return result
