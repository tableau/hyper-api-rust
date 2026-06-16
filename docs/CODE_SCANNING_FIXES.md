# Code Scanning Alerts — Resolution Notes

Resolved all 25 GitHub CodeQL code scanning alerts in a single commit
plus API dismissals (2026-06-15).

## Workflow Permissions (14 alerts)

**Rule:** `actions/missing-workflow-permissions`

All four workflow files (`ci.yml`, `release.yml`, `npm-build-publish.yml`,
`verify-hyperd-pin.yml`) lacked an explicit `permissions:` block, causing
GitHub to grant the default (overly broad) token permissions.

**Fix:** Added `permissions: { contents: read }` at the workflow level in
each file. This is the minimal set — all jobs only checkout code, run
builds/tests, or poll the check-runs API (which `contents: read` covers).

## Path Injection & Rate Limiting (6 alerts)

**Rules:** `js/path-injection`, `js/missing-rate-limiting`

The `hyper-explorer` example server (`hyperdb-api-node/examples/`) accepts
user-supplied filesystem paths in its `/api/browse` and `/api/generate`
endpoints. CodeQL flags this as path injection.

**Verdict:** By design. hyper-explorer is a localhost-only development tool
whose entire purpose is letting users browse and create `.hyper` files
anywhere on their machine. Rate limiting is similarly irrelevant for a
local tool.

**Fix:** Added `// lgtm[js/path-injection]` and
`// lgtm[js/missing-rate-limiting]` suppression comments with explanatory
notes on the flagged lines.

## Hard-coded Cryptographic Values (5 alerts)

**Rule:** `rust/hard-coded-cryptographic-value`

All five alerts pointed to `#[cfg(test)]` modules containing test fixtures:
- `auth.rs` — MD5 password test vector from PostgreSQL docs
- `config.rs` — builder test with `"mypass"` literal
- `connection.rs` — test mock construction
- `pool.rs` — pool config builder test with `"pass"` literal

**Verdict:** False positive. These are test inputs, not secrets.

**Fix:** Dismissed via the GitHub code-scanning API with reason
`"used in tests"`.
