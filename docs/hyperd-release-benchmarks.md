# hyperd release benchmark tracker

Performance history for the pinned `hyperd` engine **and** for the API code that
drives it. Rows come from an A/B run of the unified suite
([`hyperdb-api/benches/benchmark_suite.rs`](../hyperdb-api/benches/benchmark_suite.rs),
registered as a `cargo run --example` target) against the previous row.

Add a row on **either** trigger:

1. **A `hyperd` pin bump** — the pin in
   [`hyperdb-bootstrap/hyperd-version.toml`](../hyperdb-bootstrap/hyperd-version.toml)
   changes. Procedure: the
   [`update-hyperd-release`](../.claude/skills/update-hyperd-release/SKILL.md)
   skill, step 8.
2. **A material API change at an unchanged `hyperd`** — an edition or MSRV
   migration, a hot-path rewrite, a dependency bump that touches codegen, or
   anything else large enough that the previous row's numbers no longer
   describe this code.

**Why trigger 2 matters.** Each row is the baseline the *next* A/B measures
against. If the API changes materially without a row, the next engine bump
compares new-engine-plus-new-API against old-engine-plus-old-API and reports the
sum as an engine delta. Recording an API-only row keeps the two variables
separable, so an engine bump measures the engine.

This complements [BENCHMARK_GUIDE.md](BENCHMARK_GUIDE.md), which files results
**by platform**; this file tracks results **by version pair** so a regression or
win is attributable to whichever side introduced it.

## Methodology

- **Harness:** `benchmark_suite`, TCP transport, 4 workers.
- **Numbers below are median of ≥3 runs at 100M rows** (10M-row runs are too short
  to distinguish signal from variance).
- **Only single-connection deltas are reported as reliable.** Multi-connection
  (`× 4`) workloads throttle thermally on laptops — throughput declines across
  sequential runs — so they are excluded from the headline deltas unless the run
  was on a cooled/pinned host.
- Throughput in **M rows/s**. "Δ vs prev" compares to the row above.
- The **`API`** column records the `hyperdb-api` version the row was measured
  at, so an engine delta and an API delta are never conflated. It was added
  retroactively on 2026-09-04; rows predating it are marked `0.7.x` because
  per-API-version numbers were not tracked before 1.0.0.

## Insert (single-connection, M rows/s)

| Release | Build | API | Date | Machine | Inserter (sync) | ChunkSender (sync) | AsyncArrowInserter | Δ vs prev | Notes |
|---|---|---|---|---|---:|---:|---:|---|---|
| 0.0.25080 | r2bfd835b | 0.7.x | (baseline) | M-series (thermal, laptop) | 26.87 | 26.10 | 30.01 | — | Prior pin; measured as A/B baseline during the 0.0.26225 bump. |
| 0.0.26225 | rbf04a855 | 0.7.x | 2026-08-07 | M-series (thermal, laptop) | 24.94 | 24.67 | 29.95 | sync insert −5–7%; async ~flat | See PR #219 (never shipped — held by the macOS-14 deadlock). |
| 0.0.26359 | r07abb490 | 0.7.x | 2026-08-24 | M-series (thermal, laptop) | 24.12 | 24.62 | 30.07 | vs live 0.0.25080: Inserter −11%, ChunkSender −6%, async −4% | **Shipped fix for the macOS-14 deadlock that held 0.0.26225** (PR #237). Same-session 0.0.25080 A/B baseline: 27.17 / 26.12 / 31.33. Insert path carries cold-start variance and the new engine ran second, so treat the small insert deltas as soft. |
| 0.0.26359 | r07abb490 | 1.0.0-rc.1 | 2026-09-05 | M3 Max 14-core (thermal, laptop) | 25.43 | 24.52 | 30.35 | ~flat vs the `0.7.x` row above | A/B baseline leg for the 0.0.26479 bump, re-measured on the *current* API so the engine delta below is not confounded by the 0.7.x → 1.0.0-rc.1 API change. It reproduces the 2026-08-24 row within ~1–5%, which is the evidence that the API change did not move these numbers. |
| 0.0.26479 | r96880f6a | 1.0.0-rc.1 | 2026-09-05 | M3 Max 14-core (thermal, laptop) | 25.46 | 24.86 | **68.90** | **AsyncArrowInserter +127%**; Inserter +0.1%, ChunkSender +1.4% | Engine leg. Median of 5 runs per engine, **interleaved** (old, new, old, new, …) so thermal drift loads both sides equally. The async Arrow gain is not a noise artifact despite a 23–35% per-workload spread: the two sample ranges are **disjoint** (old 24.51–31.43, new 45.56–69.95), and the same jump reproduces at 10M rows (28.22 → 49.39, +75%, also disjoint). Sync insert paths are untouched. Multi-connection (`× 4`) deltas are withheld per Methodology — measured spread 17–41% on this host with deltas of *both* signs at different scales (`AsyncArrowInserter × 4` read +23% at 100M and −20% at 10M in the same session), so they carry no signal. |

## Query (single-connection, M rows/s)

| Release | Build | API | Date | Machine | full_scan (sync) | full_scan (async) | filtered (sync) | filtered (async) | Δ vs prev | Notes |
|---|---|---|---|---|---:|---:|---:|---:|---|---|
| 0.0.25080 | r2bfd835b | 0.7.x | (baseline) | M-series (thermal, laptop) | 18.79 | 18.73 | 33.23 | 27.05 | — | Prior pin. |
| 0.0.26225 | rbf04a855 | 0.7.x | 2026-08-07 | M-series (thermal, laptop) | 31.23 | 25.10 | 32.89 | 27.18 | **full_scan +66% sync / +34% async**; filtered ~flat | Large win on the dominant query path. All 1485 workspace tests pass; identical query results. Never shipped (macOS-14 deadlock). |
| 0.0.26359 | r07abb490 | 0.7.x | 2026-08-24 | M-series (thermal, laptop) | 31.40 | 24.91 | 33.14 | 27.04 | **full_scan +67% sync / +33% async** vs live 0.0.25080; filtered ~flat | The full_scan win from the 0.262xx engine line survives into the deadlock-fixed build (PR #237). Same-session 0.0.25080 A/B baseline: 18.82 / 18.68 / 33.56 / 26.24. All 1485 tests pass; macOS-14 CI green (no deadlock). |
| 0.0.26359 | r07abb490 | 1.0.0-rc.1 | 2026-09-05 | M3 Max 14-core (thermal, laptop) | 31.29 | 24.82 | 33.54 | 26.57 | ~flat vs the `0.7.x` row above | A/B baseline leg for the 0.0.26479 bump, re-measured on the current API. Reproduces the 2026-08-24 row within ~1.5% on every column. |
| 0.0.26479 | r96880f6a | 1.0.0-rc.1 | 2026-09-05 | M3 Max 14-core (thermal, laptop) | 31.08 | 24.88 | 33.75 | 26.73 | **flat: −0.7% / +0.2% / +0.6% / +0.6%** | Engine leg, median of 5 interleaved runs. No query path moved: every delta is inside the ≤1.9% run-to-run spread these four workloads showed in the same session, so the query side of this bump is a no-op. The bump's one real change is on the insert side (see the table above). All 1586 workspace tests pass against this engine. |

## How to add a row

**On a `hyperd` pin bump:** follow the
[`update-hyperd-release`](../.claude/skills/update-hyperd-release/SKILL.md)
skill (step 8). Run the A/B against the row above, then append one insert row
and one query row with median single-connection numbers, the machine, and any
caveat worth recording. Carry the `API` value forward unchanged.

**On a material API change at an unchanged `hyperd`:** same A/B procedure, but
the two legs are *code* revisions rather than engine builds — benchmark the
previous commit and the new one back to back in the same session, on the same
machine. Repeat the `Release` and `Build` values from the row above (the engine
did not change) and set `API` to the new version. Say in Notes what changed and
why it could plausibly move throughput, so a later reader can tell an accepted
cost from an unexplained one.

Either way the row is only meaningful if it follows the Methodology above —
median of ≥3 runs at 100M rows, TCP, 4 workers, single-connection figures only.
A 10M-row run is too short to distinguish signal from variance and must not be
recorded here.
