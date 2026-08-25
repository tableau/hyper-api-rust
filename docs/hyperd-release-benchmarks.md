# hyperd release benchmark tracker

Per-release performance history for the pinned `hyperd` engine. Every time the pin
in [`hyperdb-bootstrap/hyperd-version.toml`](../hyperdb-bootstrap/hyperd-version.toml)
is bumped, add a row here from an A/B run of the unified suite
([`hyperdb-api/benches/benchmark_suite.rs`](../hyperdb-api/benches/benchmark_suite.rs))
against the previous pin. The procedure lives in the
[`update-hyperd-release`](../.claude/skills/update-hyperd-release/SKILL.md) skill.

This complements [BENCHMARK_GUIDE.md](BENCHMARK_GUIDE.md), which files results
**by platform**; this file tracks results **by release** so a regression or win
introduced by an engine bump is visible over time.

## Methodology

- **Harness:** `benchmark_suite`, TCP transport, 4 workers.
- **Numbers below are median of ≥3 runs at 100M rows** (10M-row runs are too short
  to distinguish signal from variance).
- **Only single-connection deltas are reported as reliable.** Multi-connection
  (`× 4`) workloads throttle thermally on laptops — throughput declines across
  sequential runs — so they are excluded from the headline deltas unless the run
  was on a cooled/pinned host.
- Throughput in **M rows/s**. "Δ vs prev" compares to the release in the row above.

## Insert (single-connection, M rows/s)

| Release | Build | Date | Machine | Inserter (sync) | ChunkSender (sync) | AsyncArrowInserter | Δ vs prev | Notes |
|---|---|---|---|---:|---:|---:|---|---|
| 0.0.25080 | r2bfd835b | (baseline) | M-series (thermal, laptop) | 26.87 | 26.10 | 30.01 | — | Prior pin; measured as A/B baseline during the 0.0.26225 bump. |
| 0.0.26225 | rbf04a855 | 2026-08-07 | M-series (thermal, laptop) | 24.94 | 24.67 | 29.95 | sync insert −5–7%; async ~flat | See PR #219 (never shipped — held by the macOS-14 deadlock). |
| 0.0.26359 | r07abb490 | 2026-08-24 | M-series (thermal, laptop) | 24.12 | 24.62 | 30.07 | vs live 0.0.25080: Inserter −11%, ChunkSender −6%, async −4% | **Shipped fix for the macOS-14 deadlock that held 0.0.26225** (PR #237). Same-session 0.0.25080 A/B baseline: 27.17 / 26.12 / 31.33. Insert path carries cold-start variance and the new engine ran second, so treat the small insert deltas as soft. |

## Query (single-connection, M rows/s)

| Release | Build | Date | Machine | full_scan (sync) | full_scan (async) | filtered (sync) | filtered (async) | Δ vs prev | Notes |
|---|---|---|---|---:|---:|---:|---:|---|---|
| 0.0.25080 | r2bfd835b | (baseline) | M-series (thermal, laptop) | 18.79 | 18.73 | 33.23 | 27.05 | — | Prior pin. |
| 0.0.26225 | rbf04a855 | 2026-08-07 | M-series (thermal, laptop) | 31.23 | 25.10 | 32.89 | 27.18 | **full_scan +66% sync / +34% async**; filtered ~flat | Large win on the dominant query path. All 1485 workspace tests pass; identical query results. Never shipped (macOS-14 deadlock). |
| 0.0.26359 | r07abb490 | 2026-08-24 | M-series (thermal, laptop) | 31.40 | 24.91 | 33.14 | 27.04 | **full_scan +67% sync / +33% async** vs live 0.0.25080; filtered ~flat | The full_scan win from the 0.262xx engine line survives into the deadlock-fixed build (PR #237). Same-session 0.0.25080 A/B baseline: 18.82 / 18.68 / 33.56 / 26.24. All 1485 tests pass; macOS-14 CI green (no deadlock). |

## How to add a release

Follow the [`update-hyperd-release`](../.claude/skills/update-hyperd-release/SKILL.md)
skill (step 8). In short: run the A/B, then append one insert row and one query row
with median single-connection numbers, the machine, and any caveat worth recording.
