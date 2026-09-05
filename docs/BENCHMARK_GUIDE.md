# Hyper API Benchmark Guide

Canonical benchmark harness for the pure-Rust `hyperdb-api` crate and
the `hyperdb-api-node` bindings. The one benchmark everyone should run
first is the **unified Rust suite** at
[`hyperdb-api/benches/benchmark_suite.rs`](../hyperdb-api/benches/benchmark_suite.rs):
it measures sync and async insert / query paths against the same
schema in one run and emits a side-by-side comparison table.

For cross-language comparison there's also the **Node.js bench** at
[`hyperdb-api-node/__test__/benchmark.mjs`](../hyperdb-api-node/__test__/benchmark.mjs)
that uses the identical schema so its numbers go directly into the
same tables. The Rust-side specialized benches (`benchmark.rs`,
`arrow_batching_benchmark.rs`, `grpc_benchmark_tests.rs`,
`async_parallel_benchmark.rs`) are "dig-deeper" references for
specific questions.

All benchmarks share the same schema so numbers compare directly:

```
measurements(id INT NOT NULL, sensor_id INT, value DOUBLE, timestamp BIGINT)
-- 24 bytes / row
```

The shared primitives (`ResourceStats`, `HostEnv`, formatting,
deterministic row generators) live in
[`hyperdb-api/benches/common.rs`](../hyperdb-api/benches/common.rs) and
are pulled into each bench via
`#[path = "common.rs"] mod common;`.

---

## Running the benchmark suite

```sh
export HYPERD_PATH=/path/to/hyperd

# Default: 10M rows per workload, 4 parallel workers
cargo run -p hyperdb-api --release --example benchmark_suite

# Custom: (rows per workload) (parallel workers)
cargo run -p hyperdb-api --release --example benchmark_suite -- 100000000 4

# Switch transport without rebuilding (default = TCP):
#   ipc | IPC | pipe -> Named Pipe (Windows) / Unix Domain Socket (Unix)
BENCH_TRANSPORT=ipc \
  cargo run -p hyperdb-api --release --example benchmark_suite -- 100000000 4
```

The banner under `Configuration:` prints `Transport: Tcp` or `Transport: Ipc`
so the saved `benchmark_suite.md` always records which transport produced
the numbers.

The suite prints a live log and, at the end, writes two artifacts
under `test_results/`:

- `benchmark_suite.md` — markdown table identical to the one this
  doc shows for each platform.
- `benchmark_suite.json` — machine-readable version with full host
  and record fields for offline analysis.

### Matrix the suite covers

| Workload | Flavor | Variant |
|---|---|---|
| `insert.bulk` | sync | `Inserter` (HyperBinary), 1 connection |
| `insert.bulk` | sync | `ChunkSender × N`, 1 connection, N threads |
| `insert.bulk` | async | `AsyncArrowInserter`, 1 connection |
| `insert.bulk` | async | `AsyncArrowInserter × N`, N connections |
| `insert.bulk` | async | `spawn_blocking + ChunkSender × N`, N connections |
| `query.full_scan` | sync / async / parallel async | `SELECT id, sensor_id, value, timestamp FROM measurements` |
| `query.filtered` | sync / async / parallel async | `WHERE sensor_id = 5` |
| `query.aggregation` | sync / async / parallel async | `GROUP BY sensor_id` |

Parallel async queries run against the database populated by the
parallel-Arrow insert (one table per worker), so the full-scan row
count is *N × rows-per-workload*.

### Other benches (deep-dive)

| Bench | What it isolates |
|---|---|
| [`benchmark.rs`](../hyperdb-api/benches/benchmark.rs) | Sync single- vs multi-threaded insert, per-query resource stats, IPC vs TCP, TCP vs gRPC on a synthesized 4-column schema |
| [`arrow_batching_benchmark.rs`](../hyperdb-api/benches/arrow_batching_benchmark.rs) | Arrow IPC flush-threshold sweep (1 B / 16 MB / 64 MB), sync + async, IPC vs TCP |
| [`grpc_benchmark_tests.rs`](../hyperdb-api/benches/grpc_benchmark_tests.rs) | gRPC `SYNC` / `ADAPTIVE` / `ASYNC` transfer modes at 10K / 100K / 100M row scales |
| [`async_parallel_benchmark.rs`](../hyperdb-api/benches/async_parallel_benchmark.rs) | Parallel AsyncArrowInserter vs spawn-blocking ChunkSender, parallel streaming queries in 3 shapes |
| [`hyperdb-api-node/__test__/benchmark.mjs`](../hyperdb-api-node/__test__/benchmark.mjs) | Node.js N-API bindings — eager / streaming / chunked / columnar / Arrow IPC variants |

Running any of these:

```sh
cargo run -p hyperdb-api --release --example benchmark          [ROWS]
cargo run -p hyperdb-api --release --example arrow_batching_benchmark  [ROWS]
cargo run -p hyperdb-api --release --example grpc_benchmark_tests
cargo run -p hyperdb-api --release --example async_parallel_benchmark [ROWS] [WORKERS]
cargo run -p hyperdb-api --release --example benchmark_suite          [ROWS] [WORKERS]
```

### Running the Node.js bench

The Node.js bench exercises the `hyperdb-api-node` N-API bindings on
the same `measurements` schema, so its numbers go in the same
tables as the Rust suite. It additionally covers paths that only
exist in the Node API — eager `executeQuery`, streaming
`executeQueryStream`, `executeQueryColumnar` (Arrow-columnar fast
path), and `executeQueryToArrow` (full Arrow IPC roundtrip).

```sh
cd hyperdb-api-node
npm install                   # first time only
npm run build                 # builds hyperdb-api-node.<platform>.node
HYPERD_PATH=/path/to/hyperd node __test__/benchmark.mjs [ROWS]
```

Default is 1M rows. 10M matches the Rust suite's default for
cross-language comparison. 100M is feasible for insert but runs
out of V8 heap on eager-materialization queries (this is a real
characteristic of going through JS-object boundaries; use the
Columnar or Arrow variants for large reads).

---

## Results by platform

Results are filed per platform with hardware/software provenance so
numbers can be compared apples-to-apples. Each platform section has
three subsections:

1. **Rust suite** at 100M rows per workload, 4 parallel workers.
2. **Node.js bench** at 10M rows (larger scales OOM on the eager
   scan path; see note in that subsection).
3. **Rust vs Node.js** head-to-head at 10M rows.

Contributions welcome for additional platforms — paste the
`test_results/benchmark_suite.md` output and the Node bench
summary table under the appropriate section and include the host
block from the suite's stdout.

### Platform: macOS (Apple Silicon)

**Hardware / software**

- **OS:** Darwin 26.6.2 (aarch64)
- **CPU:** Apple M3 Max (14 physical / 14 logical cores)
- **Memory:** 96.0 GB
- **Rust:** rustc 1.98.0 (88d9e12ae 2026-08-18)
- **Node.js:** v24.18.0 (for the hyperdb-api-node bench)
- **hyperdb-api version:** 1.0.0
- **hyperd:** `0.0.26479.r96880f6a` (the pin in `hyperd-version.toml`, arm64 native)
- **Date:** 2026-09-05 (Rust suite: median of 5 runs; Node bench: median of 15)

> **Partial re-measure at the `0.0.26479` pin.** The Rust tables below were
> first collected at `0.0.26359`, then re-run as an interleaved A/B against
> `0.0.26479` (median of 5 runs per engine, at both 100M and 10M). Exactly one
> workload moved: **`AsyncArrowInserter`, single connection**, whose rows are
> restated below. Every other single-connection figure reproduced within ±2%,
> and the `× 4` figures within their own much wider spread, so those rows are
> carried forward from the `0.0.26359` session rather than replaced by a fresh
> sample differing only by noise — re-rolling them would have published, for
> instance, a spurious −18% on `query.full_scan × 4`. Per-release history lives
> in [hyperd-release-benchmarks.md](hyperd-release-benchmarks.md). The Node.js
> figures further down were **not** re-measured and remain at `0.0.26359`.

#### Rust suite — 100M rows per workload, 4 parallel workers

| Workload | Variant | Flavor | Rows | Time (s) | Rows/sec | MB/sec |
|---|---|---|---:|---:|---:|---:|
| insert.bulk | AsyncArrowInserter | async | 100.00M | 1.451 | 68.90 M/s | 1653.6 |
| insert.bulk | AsyncArrowInserter × 4 | async | 100.00M | 2.063 | 48.47 M/s | 1163.4 |
| insert.bulk | ChunkSender × 4 | sync | 100.00M | 4.049 | 24.70 M/s | 592.8 |
| insert.bulk | Inserter (HyperBinary) | sync | 100.00M | 3.998 | 25.01 M/s | 600.3 |
| insert.bulk | spawn_blocking+ChunkSender × 4 | async | 100.00M | 2.392 | 41.81 M/s | 1003.4 |
| query.aggregation | 4 parallel connections | async | 40 | 0.109 | 368/s | 0.0 |
| query.aggregation | single connection | sync | 10 | 0.050 | 199/s | 0.0 |
| query.aggregation | single connection | async | 10 | 0.049 | 205/s | 0.0 |
| query.filtered | 4 parallel connections | async | 10.00M | 0.207 | 48.31 M/s | 579.8 |
| query.filtered | single connection | sync | 10.00M | 0.301 | 33.20 M/s | 398.4 |
| query.filtered | single connection | async | 10.00M | 0.372 | 26.90 M/s | 322.8 |
| query.full_scan | 4 parallel connections | async | 100.00M | 1.361 | 73.45 M/s | 1762.9 |
| query.full_scan | single connection | sync | 100.00M | 3.218 | 31.08 M/s | 745.8 |
| query.full_scan | single connection | async | 100.00M | 4.014 | 24.91 M/s | 597.9 |

> **Read the `× 4` rows as order-of-magnitude only.** Measured over 5 runs on
> this host, the multi-connection variants have a run-to-run spread of
> **±20–61%**, because 4 workers contend on a 14-core laptop. The
> single-connection rows are the ones to compare across releases: all but one
> are stable to within ±2.4%. The exception is `AsyncArrowInserter`, which
> swings ±25–35% run to run even on one connection — compare it only via
> medians of several runs, and only against another median.

**Headline takeaways (Rust, macOS / M3 Max):**

- **Parallel reads are the standout** — `query.full_scan × 4` reaches roughly **73 M rows/s / 1763 MB/s**, very approximately 2× the single-connection sync scan. Per the note above these are order-of-magnitude figures, so do not read a precise speedup ratio out of them; the single-connection rows are the ones with a tight enough spread to compare.
- **Parallelism no longer helps Arrow inserts.** Since the `0.0.26479` engine, single-connection `AsyncArrowInserter` (68.9 M rows/s) outruns `AsyncArrowInserter × 4`, so spending connections on an Arrow insert buys nothing on this host.
- **Sync beats async on single-connection reads.** `query.full_scan` sync runs 31.1 M rows/s against async's 24.9 M rows/s, and `query.filtered` 33.2 vs 26.9 M rows/s. Async wins only once it can use multiple connections, so prefer the sync path for a single streaming consumer and reach for async when you have concurrency to exploit.
- **Async dominates single-connection *inserts*** — `AsyncArrowInserter` at 68.9 M rows/s versus sync `Inserter` at 25.5 M rows/s, a 2.7× gap. This is the one figure the `0.0.26479` engine bump moved: **+127%** (30.4 → 68.9 M rows/s), reproduced as **+75%** at 10M. Both are medians of 5 interleaved runs whose old and new ranges do not overlap, so the gain survives this workload's wide ±25–35% spread. Sync inserts were unaffected.
- **Single-connection scans are much faster than the previous entry** (18.8 → 31.1 M rows/s sync full-scan). Note this is *not* a controlled comparison: the prior numbers were taken on a different `hyperd`, rustc 1.94, and macOS 26.4, so the gain cannot be attributed to any single change.

#### Node.js bench — 10M rows (same schema)

Run via `HYPERD_PATH=… node __test__/benchmark.mjs 10000000`. The
`Columnar` and `Arrow IPC` variants exist only in the Node API and
are the fastest ways to move data out of the JS boundary. The
eager-object path (`executeQuery` returning `Row[]`) is the only
one that OOMs at 100M rows under the default V8 heap — insert
succeeds at 100M (51 s, 2.0 M/s, 45 MB/s) but the subsequent
eager scan exhausts the heap. For large reads through
`hyperdb-api-node`, always use `executeQueryColumnar` or
`executeQueryToArrow`.

| Workload | Variant | Rows | Time (s) | Rows/sec | MB/sec |
|---|---|---:|---:|---:|---:|
| insert.bulk | RowInserter (COPY, row API) | 10.00M | 4.659 | 2.15 M/s | 51.5 |
| insert.bulk | **ArrowInserter (COPY, Arrow IPC)** | **10.00M** | **0.242** | **41.3 M/s** | **991.7** |
| query.full_scan | executeQuery (eager, 1M only) | 1.00M | 0.684 | 1.46 M/s | 35.1 |
| query.full_scan | executeQueryStream (1M only) | 1.00M | 1.371 | 729 K/s | 17.5 |
| query.full_scan | executeQueryStream (chunked, 1M only) | 1.00M | 0.746 | 1.34 M/s | 32.2 |
| query.full_scan | **executeQueryColumnar** | **1.00M** | **0.076** | **13.2 M/s** | **315.8** |
| query.full_scan | **executeQueryToArrow** | **1.00M** | **0.035** | **28.6 M/s** | **685.7** |
| query.filtered | executeQueryStream (sensor_id=5) | 100K | 0.176 | 568 K/s | 13.6 |
| query.filtered | executeQueryColumnar | 100K | 0.012 | 8.33 M/s | 200.0 |
| query.filtered | **executeQueryToArrow** | **100K** | **0.005** | **20.0 M/s** | **480.0** |
| query.aggregation | GROUP BY sensor_id | 1.00M | 0.006 | 167 M/s | — |

> **Three of these paths are bimodal on this host**, which is why the table is
> a median of 15 runs rather than 5. `executeQueryToArrow` on the filtered
> query lands at either ~0.005 s or ~0.18 s (10 of 15 runs fast);
> `executeQueryStream` full-scan splits between ~0.79 s and ~1.4 s; and the
> filtered stream between ~0.11 s and ~0.18 s. A 5-run sample can put the
> median in either mode, so treat single short runs of these three as
> unreliable. The sub-10 ms measurements in particular are dominated by one
> GC pause or JIT decision.

#### Rust suite — 10M rows per workload, 4 parallel workers

Same host and `hyperd` as the 100M table above, collected separately on
2026-09-05 (median of 5 runs). This exists so the Rust-vs-Node comparison
below is checkable: that comparison runs both harnesses at 10M, and quoting
Rust figures with only a 100M table published made them impossible to verify.

| Workload | Variant | Flavor | Rows | Time (s) | Rows/sec | MB/sec |
|---|---|---|---:|---:|---:|---:|
| insert.bulk | AsyncArrowInserter | async | 10.00M | 0.202 | 49.39 M/s | 1185.4 |
| insert.bulk | AsyncArrowInserter × 4 | async | 10.00M | 0.263 | 37.97 M/s | 911.2 |
| insert.bulk | ChunkSender × 4 | sync | 10.00M | 0.435 | 23.00 M/s | 551.9 |
| insert.bulk | Inserter (HyperBinary) | sync | 10.00M | 0.423 | 23.64 M/s | 567.4 |
| insert.bulk | spawn_blocking+ChunkSender × 4 | async | 10.00M | 0.271 | 36.86 M/s | 884.7 |
| query.aggregation | 4 parallel connections | async | 40 | 0.035 | 1 K/s | 0.0 |
| query.aggregation | single connection | async | 10 | 0.007 | 1 K/s | 0.0 |
| query.aggregation | single connection | sync | 10 | 0.007 | 1 K/s | 0.0 |
| query.filtered | 4 parallel connections | async | 1.00M | 0.042 | 23.84 M/s | 286.0 |
| query.filtered | single connection | async | 1.00M | 0.040 | 25.29 M/s | 303.5 |
| query.filtered | single connection | sync | 1.00M | 0.032 | 31.42 M/s | 377.1 |
| query.full_scan | 4 parallel connections | async | 10.00M | 0.180 | 55.43 M/s | 1330.4 |
| query.full_scan | single connection | async | 10.00M | 0.405 | 24.69 M/s | 592.6 |
| query.full_scan | single connection | sync | 10.00M | 0.322 | 31.03 M/s | 744.6 |

#### Rust vs Node.js — 10M apples-to-apples

Same schema, same dataset shape, **both harnesses run at 10M rows** so the
figures line up. Each side's timed region is end-to-end and includes
generating the rows: the Node Arrow path pays for filling typed arrays,
building the Arrow table, and IPC-serializing it, all inside the measurement.

| Workload | Rust (best) | Node (best) | Rust factor |
|---|---|---|---:|
| insert.bulk | **AsyncArrowInserter (1 conn) — 49.39 M/s / 1185.4 MB/s** | ArrowInserter — 41.3 M/s / 991.7 MB/s | **1.2×** |
| insert.bulk (row API) | sync Inserter — **23.64 M/s / 567.4 MB/s** | RowInserter — 2.15 M/s / 51.5 MB/s | ~11× (CPU-bound JS encode) |
| query.full_scan | async × 4 — **55.43 M/s / 1330.4 MB/s** | executeQueryToArrow — 28.6 M/s / 685.7 MB/s | 1.9× |
| query.filtered | sync — **31.42 M/s / 377.1 MB/s** | executeQueryToArrow — 20.0 M/s / 480.0 MB/s | 1.6× |
| query.aggregation | sync — ~1 K/s | GROUP BY — 167 M/s | — (server-side; both latency-bound) |

Reading: on the **Arrow-IPC ingest path the two are within striking distance,
with Rust now ~1.2× ahead.** Note *which* Rust variant wins that row: the
single-connection `AsyncArrowInserter`, not the `× 4` one. At 10M rows the
parallel variant still pays a fixed cost to spin up 4 workers and connections
that it cannot amortize, and since the `0.0.26479` engine roughly doubled the
single-connection async Arrow path, that path is now the fastest Rust insert
at this scale outright. The honest reading of this row is "the Arrow path is
competitive from either language," not a language ranking.

> **This row mixes engine versions.** The Rust figures are at `0.0.26479`; the
> Node figures were measured at `0.0.26359` and have not been re-run. Node's
> `ArrowInserter` goes through the same `hyperd` ingest path that got faster,
> so it plausibly gains too and the `1.2×` should be read as provisional until
> the Node bench is re-run at the current pin.

On **reads** Rust keeps a genuine ~1.6–1.9× lead, since it never materializes
an Arrow table in a JS heap. And the **row-by-row API remains the one to
avoid from Node** at ~11× slower than native: that gap is JS object
materialization, and it is why `ArrowInserter` exists. The guidance is
unchanged — opt into `ArrowInserter` and `executeQueryColumnar` /
`executeQueryToArrow` for anything bulk.

### Platform: Linux (x86_64)

**Hardware / software** *(placeholder — replace with `host` block from your suite run)*

- **OS:** (e.g. Ubuntu 24.04)
- **CPU:**
- **Memory:**
- **Rust:**
- **Node.js:**
- **hyperdb-api version:**
- **hyperd:**
- **Date:**

#### Rust suite — 100M rows per workload, 4 parallel workers

*Paste the contents of `test_results/benchmark_suite.md` here after running the suite on Linux. Keep the same column order so the section renders identically across platforms.*

#### Node.js bench — 10M rows

*Paste the `SUMMARY` block from `node __test__/benchmark.mjs 10000000`. See the macOS subsection for the target table shape.*

#### Rust vs Node.js — 10M apples-to-apples

*Fill in once both Rust (at 10M) and Node (at 10M) numbers are captured.*

### Platform: Windows (x86_64, native)

**Hardware / software**

- **OS:** Windows 11 (build 26100) (x86_64)
- **CPU:** Intel(R) Core(TM) i9-10980XE @ 3.00 GHz (18 physical / 36 logical cores)
- **Memory:** 127.8 GB
- **Rust:** rustc 1.92.0 (ded5c06cf 2025-12-08)
- **Node.js:** *not yet captured*
- **hyperdb-api version:** 0.1.0-rc.1
- **hyperd:** Release build pinned via `hyperdb-bootstrap`
- **Date:** 2026-05-02

#### Rust suite — 100M rows per workload, 4 parallel workers, TCP loopback

| Workload | Variant | Flavor | Rows | Time (s) | Rows/sec | MB/sec |
|---|---|---|---:|---:|---:|---:|
| insert.bulk | AsyncArrowInserter | async | 100.00M | 18.563 | 5.39 M/s | 123.3 |
| insert.bulk | AsyncArrowInserter × 4 | async | 100.00M | 4.931 | 20.28 M/s | 464.1 |
| insert.bulk | ChunkSender × 4 | sync | 100.00M | 23.255 | 4.30 M/s | 98.4 |
| insert.bulk | Inserter (HyperBinary) | sync | 100.00M | 22.716 | 4.40 M/s | 100.8 |
| insert.bulk | spawn_blocking+ChunkSender × 4 | async | 100.00M | 4.778 | 20.93 M/s | 479.0 |
| query.aggregation | 4 parallel connections | async | 40 | 0.367 | 109/s | 0.0 |
| query.aggregation | single connection | sync | 10 | 0.180 | 56/s | 0.0 |
| query.aggregation | single connection | async | 10 | 0.179 | 56/s | 0.0 |
| query.filtered | 4 parallel connections | async | 10.00M | 0.611 | 16.37 M/s | 187.3 |
| query.filtered | single connection | sync | 10.00M | 1.263 | 7.92 M/s | 90.6 |
| query.filtered | single connection | async | 10.00M | 1.443 | 6.93 M/s | 79.3 |
| query.full_scan | 4 parallel connections | async | 100.00M | 6.003 | 16.66 M/s | 381.3 |
| query.full_scan | single connection | sync | 100.00M | 14.124 | 7.08 M/s | 162.1 |
| query.full_scan | single connection | async | 100.00M | 16.178 | 6.18 M/s | 141.5 |

**Headline takeaways (Rust, native Windows / i9-10980XE):**

- **Parallel async inserts** are the throughput-dominant path — `spawn_blocking + ChunkSender × 4` reaches **20.9 M rows/s / 479 MB/s**, ~2× faster than sync inserts and within ~30% of the TCP loopback ceiling on this box. The 4-way parallel insert numbers are roughly on par with macOS / M3 Max in absolute throughput, suggesting hyperd's ingest path is *not* the bottleneck here.
- **Single-connection sync query** went from 2.89 M/s (pre-2026-05 tuning) to **7.08 M/s** — a 2.5× improvement — after the read-window + TCP-buffer changes documented below.
- **Single-connection sync inserts on Windows lag** native Linux/macOS by ~5× even after tuning. This is a residual `hyperd`-side gap; the parallel paths hide it because they exercise multiple ingest threads.

#### Rust suite — same hardware, Named Pipe transport

Run with `BENCH_TRANSPORT=ipc` to switch the data path from TCP loopback to a
Windows Named Pipe. Both transports go through the same `hyperdb-api` API; only
the wire underneath changes.

| Workload | Variant | TCP rows/s | IPC rows/s | Δ |
|---|---|---:|---:|---:|
| insert.bulk | sync Inserter (HyperBinary) | 4.40 | **6.16** | **+40%** |
| insert.bulk | sync ChunkSender × 4 | 4.30 | **6.06** | **+41%** |
| insert.bulk | async AsyncArrowInserter | 5.39 | **7.24** | **+34%** |
| insert.bulk | async AsyncArrowInserter × 4 | **20.28** | 19.84 | -2% |
| insert.bulk | async spawn_blocking+ChunkSender × 4 | **20.93** | 20.81 | -1% |
| query.full_scan | sync | **7.08** | 5.02 | -29% |
| query.full_scan | async | **6.18** | 1.46 | **-76%** |
| query.full_scan | async × 4 | **16.66** | 4.96 | -70% |
| query.filtered | sync | **7.92** | 7.09 | -10% |
| query.filtered | async | **6.93** | 3.92 | -43% |
| query.filtered | async × 4 | **16.37** | 7.80 | -52% |

**Reading:** Named Pipe wins single-connection write-heavy paths by 34–41%
but catastrophically regresses every read-heavy path — especially async
(`query.full_scan` async drops 76%). The asymmetry localizes to tokio's
`NamedPipeClient::poll_read`: each completion-port wake-up appears to
deliver substantially less data than the corresponding `WSARecv` wake-up
on a TCP socket, multiplying per-poll overhead on long streamed reads.

**Recommendation:** keep `TransportMode::Tcp` (the workspace default) for
mixed workloads. Opt into `TransportMode::Ipc` only for insert-dominant
Windows pipelines that don't stream large query results back through the
same process.

#### Node.js bench — 10M rows

*Not yet captured on native Windows. Run via `npm install && npm run build && node __test__/benchmark.mjs 10000000` from `hyperdb-api-node/` and paste the `SUMMARY` block here.*

#### Rust vs Node.js — 10M apples-to-apples

*Fill in once both Rust (at 10M) and Node (at 10M) numbers are captured.*

### Platform: Windows (x86_64 / WSL2)

**Hardware / software** *(placeholder)*

- **OS:** (e.g. Ubuntu 22.04 under WSL2)
- **CPU:**
- **Memory:**
- **Rust:**
- **Node.js:**
- **hyperdb-api version:**
- **hyperd:**
- **Date:**

#### Rust suite — 100M rows per workload, 4 parallel workers

*Paste the contents of `test_results/benchmark_suite.md` here after running the suite under WSL2. WSL2 numbers should land near native Linux — see the [Windows notes](#windows-notes) below for context.*

#### Node.js bench — 10M rows

*Paste the `SUMMARY` block from `node __test__/benchmark.mjs 10000000`.*

#### Rust vs Node.js — 10M apples-to-apples

*Fill in once both Rust and Node numbers are captured.*

---

## Windows notes

Windows native I/O against `hyperd` historically ran roughly 6× slower
than macOS / Linux on the streaming-query paths. A 2026-05 client-side
tuning pass closed that gap to roughly **2.5–3.3×** on sync full-scan
queries; the residual is hyperd-side.

If you're benchmarking on Windows:

- **For performance comparison:** WSL2 still runs faster because hyperd's
  internal hot paths perform better under Linux. Expect Linux-like
  numbers there.
- **For Windows-native validation:** run the suite directly under
  PowerShell / cmd; the numbers in the [Windows native section](#platform-windows-x86_64-native)
  above are the current state of the art post-tuning.

### Client-side tuning that landed for Windows (2026-05)

Four client-side optimizations on the loopback TCP data path, all in
`hyperdb-api-core::client`:

| Change | File / function | Why |
|---|---|---|
| Read syscall window 8 KB → 64 KB | `connection.rs::RawConnection::read_message`, `async_connection.rs::AsyncRawConnection::read_message` | Each `WSARecv` on Windows is several times more expensive than its `recv` counterpart on Linux/macOS. The default 8 KB stack-buffer ceiling caused 8× syscall amplification on long streamed reads. |
| Read directly into `BytesMut` spare capacity | same as above | Removes the temporary stack buffer + `extend_from_slice` memcpy. Safe Rust via `resize` + `truncate`. |
| `SO_RCVBUF` / `SO_SNDBUF` 64 KB → 4 MiB | `client.rs::Client::connect`, `async_client.rs::AsyncClient::connect` | Windows defaults to ~64 KiB TCP buffers, which clamps the receive window so hyperd blocks on `send()` once the kernel buffer fills. Linux auto-tunes much higher. Empirical sweep found 4 MiB is the throughput knee — 8 MiB regresses sync inserts ~18% from extra memory pressure. |
| Initial `BytesMut` capacity 8 KB → 64 KB | `connection.rs`, `async_connection.rs` (struct ctor) | Avoids early reallocation churn during the first batch of messages. |

On the same i9-10980XE / Windows 11 host the four changes together took
the single-connection sync `query.full_scan` from 2.89 to **7.08 M/s
(+145%)** and the 4-connection parallel scan from 9.31 to **16.66 M/s
(+79%)**. Inserts are unchanged within noise because their bottleneck is
hyperd's ingest CPU, not the wire.

### Open question for cross-platform validation

The 4 MiB `SO_RCVBUF` / `SO_SNDBUF` setting is empirically the right
shape on Windows, where the kernel default is tiny. **It should be at
worst neutral on macOS / Linux** because their auto-tuning kernels treat
`setsockopt` as an upper bound, not a forced size, and our request is
large enough not to clamp legitimate windows. But this hasn't been
benchmarked end-to-end on those platforms post-tuning.

If you're investigating from macOS or Linux: please run the suite at the
same 100M / 4-worker scale on `main` and confirm the numbers above the
Windows section haven't shifted. If they have, the most likely cause is
the 4 MiB `setsockopt` clamping somewhere it shouldn't — search
`set_recv_buffer_size` / `set_send_buffer_size` in
`hyperdb-api-core/src/client/{client,async_client}.rs` and consider gating those
lines on `#[cfg(target_os = "windows")]` if a regression appears.

A companion bench helper exists for transport A/B without rebuilding:

```sh
BENCH_TRANSPORT=tcp ./target/release/examples/benchmark_suite 100000000 4
BENCH_TRANSPORT=ipc ./target/release/examples/benchmark_suite 100000000 4
```

On Unix, `BENCH_TRANSPORT=ipc` switches to a Unix Domain Socket; on
Windows it switches to a Named Pipe. The Named Pipe results above show
that IPC is a *write-only* win on Windows, but on Linux UDS may net
out differently — worth measuring.

---

## Adding a platform

1. Build in release mode: `cargo build --release -p hyperdb-api --example benchmark_suite`
2. Run the suite:

   ```sh
   HYPERD_PATH=/path/to/hyperd \
     ./target/release/examples/benchmark_suite 100000000 4
   ```

3. Copy-paste:
   - The `Host:` block from stdout into the platform section as the hardware/software block.
   - The `| Workload | … |` markdown table at the end of stdout into the results block.
4. Commit both the doc update and the JSON artifact (`test_results/benchmark_suite.json`) so future runs can diff against yours.

## Tuning

- **Scale:** 100M rows is the default for the comparison tables;
  smaller scales (< 10M) don't give the parallel variants enough work
  to amortize task-spawn overhead.
- **Workers:** 4 is the default because it matches typical disk / NIC
  parallelism on a developer machine. Scale up to `num_cpus` for peak
  aggregate throughput on servers with NVMe / 10 GbE.
- **Release mode:** always. Debug mode is 5–10× slower and the
  difference is not a linear factor across the matrix, so relative
  comparisons become meaningless.

## Related docs

- [DEVELOPMENT.md](../DEVELOPMENT.md) — workspace architecture, build
  instructions, and pointers to crate-level dev guides.
- [hyperd-release-benchmarks.md](hyperd-release-benchmarks.md) — per-release
  performance history of the pinned `hyperd` engine. Where this guide files
  results *by platform*, that file tracks them *by release* so an engine bump's
  effect is visible over time. Populated by the `update-hyperd-release` skill.

## Reproducibility notes

- The suite uses `CreateAndReplace` per bench, so it leaves the DB
  files behind under `test_results/` for postmortem. They're
  gitignored.
- Parallel-async insert variants use *N* independent tables
  (`measurements_0` … `measurements_N-1`) so no connection contends
  on the same table. Parallel-async queries run against those same
  tables, one per worker.
- Every row is deterministic: `id = start + i`, `sensor_id = id % 10`,
  `value = id * 0.1`, `timestamp = 1_700_000_000_000 + id * 1000`. Two
  runs of the suite against the same hyperd produce byte-identical
  `.hyper` files.
- `HyperProcess` is created once and shared by all benchmarks in a
  single suite run. Drop order is explicit at the end of `main` so
  the tokio runtime terminates before `hyperd` to avoid shutdown
  races.
