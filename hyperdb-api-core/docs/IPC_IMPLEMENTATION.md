# IPC Transport Implementation Guide

This document describes the Unix Domain Socket (UDS) and Windows Named Pipe
implementations, and records the measured performance characteristics of IPC
versus TCP loopback.

## Overview

The Hyper Rust API supports multiple transport mechanisms:

- **TCP**: Traditional TCP/IP connections (cross-platform)
- **Unix Domain Sockets (UDS)**: High-performance IPC on Unix/macOS (implemented)
- **Windows Named Pipes**: High-performance IPC on Windows (implemented)

Both IPC transports are complete and reachable from the public API. TCP remains
the *default* data path on every platform — not because IPC is unfinished, but
because of the measured throughput trade-off documented in
[Transport Performance](#transport-performance-uds-vs-tcp-on-macos) below and in
[docs/BENCHMARK_GUIDE.md](../../docs/BENCHMARK_GUIDE.md).

## Architecture

### Core Abstraction: `ConnectionEndpoint`

Location: `hyperdb-api-core/src/client/endpoint.rs`

```rust
pub enum ConnectionEndpoint {
    /// TCP endpoint: `tab.tcp://host:port`
    Tcp { host: String, port: u16 },

    /// Unix Domain Socket: `tab.domain://<directory>/domain/<name>`
    #[cfg(unix)]
    DomainSocket { directory: PathBuf, name: String },

    /// Windows Named Pipe: `tab.pipe://<host>/pipe/<name>`
    #[cfg(windows)]
    NamedPipe { host: String, name: String },
}
```

`parse()` handles all three descriptor forms and returns a clear error when a
descriptor is used on a platform that cannot serve it (for example
`tab.pipe://` on Unix). Companion helpers: `tcp()`, `domain_socket()`,
`named_pipe()`, `to_descriptor()`, `socket_path()` (Unix), `pipe_path()`
(Windows), and the `is_*` predicates.

### Stream Abstractions

**Async Stream** (`hyperdb-api-core/src/client/async_stream.rs`):

```rust
pub enum AsyncStream {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    NamedPipe(tokio::net::windows::named_pipe::NamedPipeClient),
}
```

**Sync Stream** (`hyperdb-api-core/src/client/sync_stream.rs`):

```rust
pub enum SyncStream {
    Tcp(std::net::TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    #[cfg(windows)]
    NamedPipe(std::fs::File),
}
```

The sync named-pipe variant is a plain `std::fs::File`: on Windows a pipe handle
opened through `CreateFile` supports `ReadFile`/`WriteFile`, so `Read` and
`Write` delegate to the file directly with no Win32 bindings needed.

## Connection Descriptor Formats

The C API uses the following descriptor formats:

- TCP: `tab.tcp://host:port`
- Unix Domain Socket: `tab.domain://<directory>/domain/<socket_name>`
- Windows Named Pipe: `tab.pipe://<host>/pipe/<pipe_name>`

## Unix Domain Socket Implementation (Reference)

### Files Involved

1. **`hyperdb-api-core/src/client/endpoint.rs`** - ConnectionEndpoint enum with parsing
2. **`hyperdb-api-core/src/client/async_stream.rs`** - AsyncStream with Unix variant
3. **`hyperdb-api-core/src/client/sync_stream.rs`** - SyncStream with Unix variant
4. **`hyperdb-api-core/src/client/async_client.rs`** - `connect_unix()`, `connect_endpoint()`
5. **`hyperdb-api-core/src/client/client.rs`** - `connect_unix()`, `connect_endpoint()`
6. **`hyperdb-api-core/src/client/prepare.rs`** - Updated to use SyncStream
7. **`hyperdb-api/src/process.rs`** - TransportMode, UDS listen connection
8. **`hyperdb-api/src/transport.rs`** - TransportType::UnixSocket
9. **`hyperdb-api/src/connection_builder.rs`** - `build_unix()`
10. **`hyperdb-api/src/connection.rs`** - `connect_with_endpoint()`

### Key Implementation Details

#### 1. Stream Trait Implementations

Both `AsyncStream` and `SyncStream` implement the necessary I/O traits by delegating to the underlying stream:

```rust
// async_stream.rs
impl AsyncRead for AsyncStream { ... }
impl AsyncWrite for AsyncStream { ... }

// sync_stream.rs
impl Read for SyncStream { ... }
impl Write for SyncStream { ... }
```

#### 2. Client Connection Methods

Each client has these connection methods:

- `connect(&Config)` - TCP via Config (original)
- `connect_unix(path, &Config)` - UDS via socket path (Unix only)
- `connect_named_pipe(pipe_path, &Config)` - Named Pipe (Windows only)
- `connect_endpoint(&ConnectionEndpoint, &Config)` - Generic via endpoint

#### 3. HyperProcess Integration

In `hyperdb-api/src/process.rs`:

- `TransportMode` enum: `Ipc` or `Tcp`
- Socket directory created under the platform temp directory:
  `std::env::temp_dir().join("hyper-<pid>")` — that is `/tmp/hyper-<pid>` on
  Linux but `$TMPDIR/hyper-<pid>` (under `/var/folders/...`) on macOS
- Listen connection format: `tab.domain://<dir>/domain/hyper`
- Cleanup on drop: removes socket directory

#### 4. Connection::new() Auto-Detection

When connecting via `Connection::new(&HyperProcess, ...)`:

- Uses the process's connection endpoint if available — UDS on Unix, Named Pipe on Windows
- Falls back to string endpoint parsing (TCP)

---

## Windows Named Pipes Implementation

Named Pipe support is **implemented and tested**. This section maps the surface
to its locations; it replaces the staged implementation plan this document
carried while the work was outstanding.

### Where each piece lives

| Concern | Location | Detail |
|---|---|---|
| Endpoint variant + parsing | `hyperdb-api-core/src/client/endpoint.rs` | `ConnectionEndpoint::NamedPipe { host, name }`, `named_pipe()`, `parse()` for `tab.pipe://<host>/pipe/<name>`, `pipe_path()`, `is_named_pipe()` |
| Sync stream | `hyperdb-api-core/src/client/sync_stream.rs` | `SyncStream::NamedPipe(std::fs::File)`; `Read`/`Write` delegate to the file |
| Async stream | `hyperdb-api-core/src/client/async_stream.rs` | `AsyncStream::NamedPipe(NamedPipeClient)` from `tokio::net::windows::named_pipe`; full `AsyncRead`/`AsyncWrite` incl. `poll_write_vectored` |
| Sync connect | `hyperdb-api-core/src/client/client.rs` | `Client::connect_named_pipe()` via `OpenOptions`, plus the `NamedPipe` arm of `connect_endpoint()` and of query cancellation |
| Async connect | `hyperdb-api-core/src/client/async_client.rs` | `AsyncClient::connect_named_pipe()` via `ClientOptions::new().open()` |
| Server startup | `hyperdb-api/src/process.rs` | Emits `tab.pipe://./pipe/hyper-<pid>` for `TransportMode::Ipc`; `parse_connection_descriptor()` maps `tab.pipe://` to `\\<host>\pipe\<name>` |
| Transport detection | `hyperdb-api/src/transport.rs` | `TransportType::NamedPipe`; `detect_transport_type()` matches both `tab.pipe://` and a leading `\\` |
| Connection builders | `hyperdb-api/src/connection_builder.rs`, `async_connection_builder.rs` | `build_named_pipe()`, dispatched from `build()` on `TransportType::NamedPipe` |

### Pipe-busy retry

Both `connect_named_pipe()` implementations handle a detail the original plan
did not anticipate. Windows named pipes have a finite number of server-side
instances (`MaxInstances` on `CreateNamedPipe`); when all are busy the open
fails with `ERROR_PIPE_BUSY` (231). Rather than surfacing a spurious error, both
clients poll every 20 ms up to a 10 second deadline — the equivalent of Win32
`WaitNamedPipe` — so concurrent clients don't fail when the instance pool is
momentarily exhausted.

### Tests

- **Unit tests** in `endpoint.rs` cover `tab.pipe://` parsing (local and
  remote host), `to_descriptor()`, `pipe_path()`, `Display`, and the `is_*`
  predicates, all under `#[cfg(windows)]`.
- **Unit tests** in `process.rs` cover `parse_connection_descriptor()` for both
  `tab.pipe://./pipe/...` and `tab.pipe://<server>/pipe/...`.
- **Benchmarks**: `BENCH_TRANSPORT=ipc` switches the suite to Named Pipes on
  Windows and to UDS on Unix. Measured results for both are in
  [docs/BENCHMARK_GUIDE.md](../../docs/BENCHMARK_GUIDE.md).

## Key Crates for Windows

- `tokio` - provides `tokio::net::windows::named_pipe` (used for the async client)
- No Win32 binding crate is required: the sync path uses `std::fs::File`

## Named Pipe Format Reference

Windows named pipe paths:

- Local: `\\.\pipe\<pipe_name>`
- Remote: `\\<server>\pipe\<pipe_name>`

Hyper descriptor format:

- `tab.pipe://./pipe/<name>` (local)
- `tab.pipe://<server>/pipe/<name>` (remote)

## Build Verification

```powershell
cargo build -p hyperdb-api-core -p hyperdb-api
cargo test -p hyperdb-api-core -p hyperdb-api
cargo run -p hyperdb-api --release --example benchmark -- 100000
```

## Notes

- Default transport is `TransportMode::Tcp` on every platform. Note the
  subtlety: the `TransportMode` enum's own `#[default]` is `Ipc`, but
  `HyperProcess::start_server()` explicitly falls back to
  `TransportMode::Tcp` when no transport mode is set, so the effective default
  is TCP. See [Transport Performance](#transport-performance-uds-vs-tcp-on-macos).
- The callback connection remains TCP (simpler, cross-platform)
- Most tests simply inherit the TCP default; `hyperdb-api/tests/stress_test/simulation.rs`
  is the one place that sets `TransportMode::Tcp` explicitly

---

## Transport Performance: UDS vs TCP on macOS

**Status:** Measured 2026-09-06. Supersedes an earlier unquantified claim.

### What the earlier version of this document said

This section previously read *"Initial benchmarks on macOS showed UDS
performing slower than TCP loopback, contrary to expectations"* and was marked
"pending further investigation". That claim was **never quantified** — no
figures, no methodology, no date, and no committed harness — yet it was the
stated basis for the `TransportMode::Tcp` default.

A controlled measurement **does not reproduce it for latency-bound workloads**:
UDS is meaningfully *faster* to connect and to round-trip a small query. It does
reproduce for bulk streaming, where UDS is slower and the penalty saturates
near +62%. The blanket statement was wrong; the shape of the trade-off is real.

### Methodology

- **Host:** macOS 25.6, Apple M3 Max
- **Build:** release, isolated `CARGO_TARGET_DIR`, `HYPERD_PATH=~/dev/bin/hyperd`
- One `HyperProcess` per transport; 3 warm-up iterations discarded, then 20–30
  measured iterations
- Each iteration establishes a fresh `Connection`, runs its statement, and
  drains the result set
- Percentiles are **nearest-rank**
- Five independent runs agreed on direction
- The 100M-row insert suite was **not** run, so these figures do not speak to
  bulk-insert throughput

### Latency and small-fetch workloads

Representative run, 25 iterations, milliseconds:

| Workload | TCP median | TCP p95 | UDS median | UDS p95 | Δ median |
|---|---:|---:|---:|---:|---:|
| connect | 0.979 | 1.377 | 0.689 | 0.868 | -29.7% |
| `SELECT 1` round-trip | 0.342 | 0.479 | 0.278 | 0.384 | -18.6% |
| fetch 1,000 rows | 0.848 | 1.109 | 0.808 | 0.977 | -4.7% |
| fetch 50,000 rows | 5.252 | 5.605 | 6.044 | 7.084 | +15.1% |

Negative Δ means UDS is faster. Across the five runs the spreads were: connect
-16% to -30%, small query -11% to -29%, 1,000-row fetch -5% to -15%, and
50,000-row fetch +7% to +15%.

### Streaming crossover

20 iterations each, medians in milliseconds:

| Rows | TCP | UDS | Δ |
|---|---:|---:|---:|
| 10,000 | 1.900 | 1.920 | +1.0% |
| 50,000 | ~5.5 | ~6.1 | ~+11% |
| 200,000 | 8.049 | 13.042 | +62.0% |
| 1,000,000 | 39.395 | 63.822 | +62.0% |

The crossover sits just above 10,000 rows, and the penalty stops growing at
roughly +62% — it saturates rather than scaling with row count.

### Conclusion

**IPC wins latency, loses bulk streaming, and the streaming penalty
saturates around +62%.**

This is the same story the *measured* Windows Named Pipe table in
[docs/BENCHMARK_GUIDE.md](../../docs/BENCHMARK_GUIDE.md#rust-suite--same-hardware-named-pipe-transport)
tells: Named Pipes win single-connection inserts by 34–41% while regressing
reads (`query.full_scan` async -76%, sync -29%). Two platforms, two IPC
mechanisms, one conclusion — IPC favours short, latency-sensitive exchanges and
penalises long streamed reads. Treat the two data sets as one finding, not two
unrelated results.

Practical consequence: keeping `TransportMode::Tcp` as the default is the right
call for mixed workloads, and it is now a *measured* choice rather than a
placeholder awaiting validation. Opting into `TransportMode::Ipc` is
worthwhile for connection-heavy or small-query-heavy workloads — an MCP-shaped
request/response pattern is the clearest example — and not for pipelines that
stream large result sets.

### Untested hypotheses for the streaming regression

Everything in this subsection is **speculation that has not been measured**.
It is retained because it is a reasonable place for a future investigation to
start, not because any of it has been demonstrated. Earlier revisions of this
document presented some of these as findings, with an "Expected Impact" column
of invented percentages; those numbers were guesses and have been removed.

1. **macOS TCP loopback is highly optimized.** The XNU kernel has specialized
   fast paths for `127.0.0.1` that bypass most of the TCP/IP stack and perform
   direct memory copies between socket buffers.
2. **Socket buffer sizes are not tuned for UDS.** This one is at least
   *verifiable* in the source: `Client::connect` (and its async twin) sets
   `SO_RCVBUF`/`SO_SNDBUF` to 4 MiB on the TCP path, but `connect_unix` sets no
   socket options at all. macOS UDS defaults (`net.local.stream.sendspace` /
   `recvspace`) are far smaller than the TCP loopback defaults, so the same
   volume of data costs more syscalls. Whether raising them closes the gap is
   untested.
3. **macOS UDS sees less kernel optimization attention** than Linux UDS, which
   is heavily tuned for container and init-system workloads.
4. **Path-based socket overhead.** macOS has no abstract namespace, so every
   operation involves VFS lookups.
5. **`TCP_NODELAY` has no UDS equivalent.** `set_nodelay` is a no-op for the
   `Unix` and `NamedPipe` variants of both stream enums. UDS has no Nagle
   algorithm to disable, but the flushing behaviour is not identical.

Directions a future investigation could take, again unmeasured: setting
`SO_SNDBUF`/`SO_RCVBUF` on the UDS path, vectored `writev`/`readv` I/O, a
buffered-writer wrapper to cut syscall count, and running the same comparison
on Linux to establish whether the streaming regression is macOS-specific.

### Diagnostic Commands

To check socket buffer sizes on macOS:

```bash
# Check system defaults
sysctl kern.ipc.maxsockbuf
sysctl net.local.stream.sendspace
sysctl net.local.stream.recvspace
sysctl net.inet.tcp.sendspace
sysctl net.inet.tcp.recvspace
```

### Illustrative buffer-tuning sketch (not implemented)

If hypothesis 2 above is pursued, this is roughly the shape the change would
take. It is **not** present in the codebase and has not been benchmarked:

```rust
#[cfg(unix)]
fn configure_unix_socket(stream: &UnixStream) -> io::Result<()> {
    // socket2 already being a dependency, prefer:
    //   socket2::SockRef::from(stream).set_send_buffer_size(256 * 1024)
    // over a raw libc::setsockopt call.
    let sock = socket2::SockRef::from(stream);
    sock.set_send_buffer_size(256 * 1024)?;
    sock.set_recv_buffer_size(256 * 1024)?;
    Ok(())
}
```
