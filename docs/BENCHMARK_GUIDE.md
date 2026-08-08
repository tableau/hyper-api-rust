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

- **OS:** Darwin 26.4 (aarch64)
- **CPU:** Apple M3 Max (14 physical / 14 logical cores)
- **Memory:** 96.0 GB
- **Rust:** rustc 1.94.0 (4a4ef493e 2026-03-02)
- **Node.js:** v25.8.1 (for the hyperdb-api-node bench)
- **hyperdb-api version:** 0.1.0-rc.1
- **hyperd:** Release build on same host
- **Date:** 2026-05-02 (median of 5 post-warmup runs; TCP `SO_RCVBUF`/`SO_SNDBUF` tuned to 4 MiB)

#### Rust suite — 100M rows per workload, 4 parallel workers

| Workload | Variant | Flavor | Rows | Time (s) | Rows/sec | MB/sec |
|---|---|---|---:|---:|---:|---:|
| insert.bulk | AsyncArrowInserter | async | 100.00M | 3.104 | 32.22 M/s | 737.4 |
| insert.bulk | AsyncArrowInserter × 4 | async | 100.00M | 1.667 | 59.99 M/s | 1373.0 |
| insert.bulk | ChunkSender × 4 | sync | 100.00M | 3.671 | 27.24 M/s | 623.5 |
| insert.bulk | Inserter (HyperBinary) | sync | 100.00M | 3.679 | 27.18 M/s | 622.2 |
| insert.bulk | spawn_blocking+ChunkSender × 4 | async | 100.00M | 2.177 | 45.94 M/s | 1051.6 |
| query.aggregation | 4 parallel connections | async | 40 | 0.109 | 365/s | 0.0 |
| query.aggregation | single connection | sync | 10 | 0.048 | 207/s | 0.0 |
| query.aggregation | single connection | async | 10 | 0.049 | 204/s | 0.0 |
| query.filtered | 4 parallel connections | async | 10.00M | 0.220 | 45.36 M/s | 519.1 |
| query.filtered | single connection | sync | 10.00M | 0.297 | 33.67 M/s | 385.3 |
| query.filtered | single connection | async | 10.00M | 0.374 | 26.74 M/s | 306.0 |
| query.full_scan | 4 parallel connections | async | 100.00M | 2.526 | 39.59 M/s | 906.1 |
| query.full_scan | single connection | sync | 100.00M | 5.317 | 18.81 M/s | 430.5 |
| query.full_scan | single connection | async | 100.00M | 5.384 | 18.57 M/s | 425.1 |

**Headline takeaways (Rust, macOS / M3 Max):**

- **Async inserts beat sync inserts** at every scale — parallel `AsyncArrowInserter × 4` is the fastest path at **1373 MB/s aggregate** (60 M rows/s), and even single-connection `AsyncArrowInserter` (32 M rows/s) edges ahead of sync `Inserter` (27 M rows/s). The sync paths themselves saw a **~9% improvement** from the 4 MiB `SO_RCVBUF`/`SO_SNDBUF` tuning landed 2026-05.
- **Parallel queries scale well** on full-scan — 4 connections reach **40 M rows/s / 906 MB/s**, a 2.1× wall-clock speedup over the single-connection sync scan.
- **The async vs sync gap closed on single-connection queries** — post-tuning, `query.filtered async` runs 27 M rows/s vs the sync path's 34 M rows/s, and full_scan is essentially tied (18.6 vs 18.8 M rows/s). The historical "async is slower single-connection" warning no longer holds at the single-consumer-filter scale. For concurrent workloads, async remains the clear win.

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

Numbers below are post-rewrite (all async-native, Inserter renamed
to `RowInserter`, new `ArrowInserter` class).

| Workload | Variant | Rows | Time (s) | Rows/sec | MB/sec |
|---|---|---:|---:|---:|---:|
| insert.bulk | RowInserter (COPY, row API) | 10.00M | 5.330 | 1.88 M/s | 42.9 |
| insert.bulk | **ArrowInserter (COPY, Arrow IPC)** | **10.00M** | **0.379** | **26.4 M/s** | **603.9** |
| query.full_scan | executeQuery (eager, 1M only) | 1.00M | 0.678 | 1.47 M/s | 33.8 |
| query.full_scan | executeQueryStream (1M only) | 1.00M | 0.801 | 1.25 M/s | 28.6 |
| query.full_scan | executeQueryStream (chunked, 1M only) | 1.00M | 0.874 | 1.15 M/s | 26.2 |
| query.full_scan | **executeQueryColumnar** | **1.00M** | **0.102** | **9.80 M/s** | **223.7** |
| query.full_scan | **executeQueryToArrow** | **1.00M** | **0.050** | **20.0 M/s** | **458.7** |
| query.filtered | executeQueryStream (sensor_id=5) | 100K | 0.114 | 878 K/s | 20.1 |
| query.filtered | executeQueryColumnar | 100K | 0.015 | 6.80 M/s | 156.3 |
| query.filtered | **executeQueryToArrow** | **100K** | **0.005** | **20.6 M/s** | **471.7** |
| query.aggregation | GROUP BY sensor_id | 10 | 0.003 | 320 M/s | — |

#### Rust vs Node.js — 10M apples-to-apples

Same schema, same dataset shape. Post-rewrite, the Node bindings
now have parity with Rust on the Arrow-ingest path because
`ArrowInserter` moves bytes directly into Hyper without any
per-row JS↔Rust conversion.

| Workload | Rust (best) | Node (best) | Rust factor |
|---|---|---|---:|
| insert.bulk | AsyncArrowInserter × 4 — **32.5 M/s / 745 MB/s** | ArrowInserter — 26.4 M/s / 603.9 MB/s | **1.2×** |
| insert.bulk (row API) | sync Inserter — **~25 M/s** (native) | RowInserter — 1.9 M/s | ~13× (CPU-bound JS encode) |
| query.full_scan | async × 4 — **36.3 M/s / 831 MB/s** | executeQueryToArrow — 20.0 M/s / 458.7 MB/s | 1.8× |
| query.filtered | sync — **25.6 M/s / 292.8 MB/s** | executeQueryToArrow — 20.6 M/s / 471.7 MB/s | 1.2× |
| query.aggregation | sync — 1.46 K/s | GROUP BY — 320 M/s | — (server-side; both latency-bound) |

Reading: on the **Arrow-IPC path** Node is within ~20% of native
Rust — the remaining gap is the IPC serialization cost in JS. On
the row-by-row API Rust is still ~13× faster because it can skip
the JS object materialization entirely. The pre-rewrite 16× insert
gap closes to ~1.2× once callers opt into `ArrowInserter`.

### Platform: Linux (x86_64)

**Hardware / software** _(placeholder — replace with `host` block from your suite run)_

- **OS:** (e.g. Ubuntu 24.04)
- **CPU:**
- **Memory:**
- **Rust:**
- **Node.js:**
- **hyperdb-api version:**
- **hyperd:**
- **Date:**

#### Rust suite — 100M rows per workload, 4 parallel workers

_Paste the contents of `test_results/benchmark_suite.md` here after running the suite on Linux. Keep the same column order so the section renders identically across platforms._

#### Node.js bench — 10M rows

_Paste the `SUMMARY` block from `node __test__/benchmark.mjs 10000000`. See the macOS subsection for the target table shape._

#### Rust vs Node.js — 10M apples-to-apples

_Fill in once both Rust (at 10M) and Node (at 10M) numbers are captured._

### Platform: Windows (x86_64, native)

**Hardware / software**

- **OS:** Windows 11 (build 26100) (x86_64)
- **CPU:** Intel(R) Core(TM) i9-10980XE @ 3.00 GHz (18 physical / 36 logical cores)
- **Memory:** 127.8 GB
- **Rust:** rustc 1.92.0 (ded5c06cf 2025-12-08)
- **Node.js:** _not yet captured_
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

_Not yet captured on native Windows. Run via `npm install && npm run build && node __test__/benchmark.mjs 10000000` from `hyperdb-api-node/` and paste the `SUMMARY` block here._

#### Rust vs Node.js — 10M apples-to-apples

_Fill in once both Rust (at 10M) and Node (at 10M) numbers are captured._

### Platform: Windows (x86_64 / WSL2)

**Hardware / software** _(placeholder)_

- **OS:** (e.g. Ubuntu 22.04 under WSL2)
- **CPU:**
- **Memory:**
- **Rust:**
- **Node.js:**
- **hyperdb-api version:**
- **hyperd:**
- **Date:**

#### Rust suite — 100M rows per workload, 4 parallel workers

_Paste the contents of `test_results/benchmark_suite.md` here after running the suite under WSL2. WSL2 numbers should land near native Linux — see the [Windows notes](#windows-notes) below for context._

#### Node.js bench — 10M rows

_Paste the `SUMMARY` block from `node __test__/benchmark.mjs 10000000`._

#### Rust vs Node.js — 10M apples-to-apples

_Fill in once both Rust and Node numbers are captured._

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
