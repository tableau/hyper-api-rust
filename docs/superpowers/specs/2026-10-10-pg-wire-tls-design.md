# PG-wire TLS for TCP connections — design

Status: approved direction (pre-1.0 audit decision 8, finding `A1:X-sec-a-02`).
Revised after plan review by two reviewers. Breaking changes are acceptable:
this lands before 1.0.0.

## Problem

The README, AGENTS.md, the core crate docs and the Node summary all advertise
TLS for the TCP transport. None of it exists. `hyperdb-api-core/src/client/tls.rs`
defines `TlsConfig`, `TlsMode` and an async-only `rustls_impl::create_connector`,
but nothing calls them: no client sends an `SSLRequest`, `Config` has no TLS
field, and no builder exposes one. Every TCP connection is plaintext, including
the password and the cancel secret.

## Goals

- Real TLS on PG-wire **TCP** connections, sync and async. The modes use the
  libpq `sslmode` names; where libpq and sqlx/tokio-postgres differ, behaviour
  follows sqlx/tokio-postgres (see [Divergences from libpq](#divergences-from-libpq)).
- Reachable from `ConnectionBuilder`, `AsyncConnectionBuilder`, both pools, and
  the Node `ConnectionBuilder`.
- Cancel requests travel over TLS when the connection does, and never fall back
  to plaintext.
- Tests against the real pinned `hyperd` (0.0.26479) with a TLS-enabled server,
  plus fake-server unit tests that observe what goes on the wire.
- Remove every over-claim from the docs.

## Non-goals

- **MCP.** `hyperdb-mcp` has no remote-endpoint tool (`attach_database` accepts
  only `kind:"local_file"`). A comment at the `AttachSource` "Future: Tcp" note in
  `hyperdb-mcp/src/attach.rs` records that a future `kind:"tcp"` must accept the
  TLS options. No MCP tool-surface change.
- **gRPC.** gRPC TLS is chosen by the `https://` scheme (tonic) and is unchanged.
  Setting a non-`Disable` mode on a gRPC endpoint is `Error::FeatureNotSupported`.
- **TLS over Unix sockets / named pipes.** Not offered. `Prefer` is satisfied by
  plaintext there; `Require`, `VerifyCa` and `VerifyFull` are
  `Error::FeatureNotSupported` instead of silently connecting in plaintext (a
  named pipe can be remote over SMB). hyperd's named-pipe listener also hangs
  after answering `'S'`.
- **Static `connect*` shortcuts.** `Connection::new(&hyper, ..)`,
  `Connection::connect*`, `AsyncConnection::connect*` and `transport.rs`'s
  `connect_tcp*` stay plaintext. TLS goes through the builders and pools only;
  their docs say so, and say that a server started with `ssl_force` is only
  reachable through them.
- `sslmode=` and friends in connection strings. `Config::from_str` **rejects**
  the libpq TLS keys with an error pointing at `.tls(..)` rather than passing
  them to the server as startup options.
- In-memory PEM, CRLs, OS trust store, `sslnegotiation=direct`. `TlsConfig` has
  private fields, so any of these can be added without a break.
- `Config::connect_timeout` is not applied to the TCP `connect()` today
  (pre-existing; recorded separately). This change only applies it to the TLS
  negotiation.

## Engine facts (measured on hyperd 0.0.26479, and read from hyper-db source)

- Enabled by `--ssl-key=PATH --ssl-certificate=PATH`, optionally
  `--ssl-force=true`. Through `Parameters::set` these are `ssl_key`,
  `ssl_certificate`, `ssl_force` (`_` maps to `-`). They are not listed in
  `hyperd --help`; they are defined in `hyper/tools/hyperd/hyperd.cpp`.
- On POSIX the key file must not be group/other accessible (mode 0600). The
  check is POSIX-only (`hyperd.cpp:667`), so Windows has no permission
  requirement. Cert and key must match and be currently valid.
- `SSLRequest` (code 80877103) is answered with one byte: `'S'` and a TLS
  handshake, or `'N'` when no certificate is configured.
- TLS 1.3 only; a TLS 1.2-only client gets alert 70.
- With `ssl_force`, a plaintext TCP startup is rejected ("only SSL connections
  are allowed"), loopback included. This is how tests prove enforcement.
- A `CancelRequest` is accepted both plaintext and after `SSLRequest`+TLS, with
  or without `ssl_force` (both return 57014). So hyperd cannot tell the tests
  which one we sent; the fake-server unit tests do.
- Client certificates: hyperd loads the server certificate file as its client
  trust store with `verify_peer` (`hyperd.cpp:684-688`). A client certificate,
  if offered, must chain to a CA in that file, or the connection fails. No
  certificate is fine. So mTLS is testable against hyperd when the certificate
  file holds `leaf + CA`.
- `HyperProcess` reports `localhost:PORT` and does not itself open a client
  connection, so `ssl_force` does not break startup. A certificate with SANs
  `localhost` and `127.0.0.1` satisfies `VerifyFull` against both host forms.

## Public API

### Core (`hyperdb_api_core::client::tls`), re-exported from `hyperdb_api`

```rust
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TlsMode {
    #[default]
    Disable,
    Prefer,
    Require,
    VerifyCa,   // renamed from VerifyCA
    VerifyFull,
}
// FromStr (Err = ParseTlsModeError) / Display:
// "disable" | "prefer" | "require" | "verify-ca" | "verify-full"

/// Public error type for `TlsMode::from_str` (core's `client::Error` must not
/// leak through `hyperdb_api`). Display names the five valid values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTlsModeError { /* private */ }

#[derive(Debug, Clone, Default)]
pub struct TlsConfig { /* private: mode, root_cert, client_cert, server_name */ }

impl TlsConfig {
    pub fn new(mode: TlsMode) -> Self;
    pub fn root_cert(self, pem_path: impl AsRef<Path>) -> Self;                    // sslrootcert
    pub fn client_cert(self, cert: impl AsRef<Path>, key: impl AsRef<Path>) -> Self; // sslcert/sslkey
    pub fn server_name(self, name: impl Into<String>) -> Self; // override host for SNI + VerifyFull
    pub fn mode(&self) -> TlsMode;
    // other getters pub(crate): no external consumer; making them public later is additive
}
impl From<TlsMode> for TlsConfig { .. }
```

Removed: `TlsConfig`'s pub fields, `verify_server`, `danger_accept_invalid_certs`,
`ca_cert` (now `root_cert`), `has_client_cert`, the `TlsMode` helper methods
encoding the old meaning, and the public `rustls_impl` module.

### Builders and pools (`hyperdb_api`)

- `ConnectionBuilder::tls(impl Into<TlsConfig>)`,
  `AsyncConnectionBuilder::tls(..)`, `PoolConfig::tls(..)`,
  `SyncPoolConfig::tls(..)`. So `.tls(TlsMode::Require)` works.
- `Connection::is_tls()` / `AsyncConnection::is_tls()` — `true` when the
  session is encrypted: PG-wire TLS was negotiated, or the gRPC endpoint is
  `https://`. With `Prefer`, this is how a caller learns the outcome.
- Re-exports: `hyperdb_api::{TlsConfig, TlsMode, ParseTlsModeError}`.
- Core `Config::with_tls(impl Into<TlsConfig>)` / `Config::tls()`.
- Core `client::Error::Tls(String)`, mapped to the existing
  `hyperdb_api::Error::Tls(String)`, whose doc becomes "TLS negotiation,
  handshake or certificate-verification failure". Unreadable PEM files and
  invalid option combinations are `Error::Config`.

### Node (`hyperdb-api-node`)

- `#[napi(string_enum)] TlsMode { Disable, Prefer, Require, VerifyCa, VerifyFull }`.
- `ConnectionBuilder.tlsMode(mode)`, `.tlsRootCert(path)`,
  `.tlsClientCert(certPath, keyPath)`, `.tlsServerName(name)`. Setting a
  sub-option without `tlsMode` is an error at `build()` (otherwise it would be
  silently dropped as `Disable`).
- `Connection.isTls` getter (like `isAlive`).
- `new HyperProcess(hyperPath?, options?)` with
  `options.parameters?: Record<string, string>`, applied through
  `Parameters::set`, so a test (or user) can start a TLS-enabled server.
- `index.d.ts` (hand-written) updated to match.

## Semantics

| Mode | SSLRequest | `'N'` reply | Server certificate check |
|---|---|---|---|
| `Disable` (default) | not sent | n/a | n/a |
| `Prefer` | sent | plaintext on the same socket | none, or chain-only if `root_cert` is set |
| `Require` | sent | `Error::Tls` | none, or chain-only if `root_cert` is set |
| `VerifyCa` | sent | `Error::Tls` | chain against `root_cert` (**required**: `Error::Config` without it) |
| `VerifyFull` | sent | `Error::Tls` | chain + hostname, against `root_cert` or else the bundled webpki (Mozilla) roots |

- When `root_cert` is given it is the **only** trust anchor.
- "None" verification still checks handshake signatures with the provider's
  algorithms; it only skips identity and chain.
- Chain-only verification calls
  `rustls::client::verify_server_cert_signed_by_trust_anchor` directly (the same
  chain step `WebPkiServerVerifier` runs before its name check), rather than
  pattern-matching name errors.
- `Prefer` falls back only on `'N'`. A handshake or verification failure is an
  error, not a downgrade.
- Hostname for SNI and `VerifyFull` = `server_name` override, else the `Config`
  host with IPv6 brackets stripped. IP hosts become `ServerName::IpAddress` (no
  SNI, IP SAN check).
- A client certificate (mTLS) is offered in any enabled mode when configured.
- Crypto provider: rustls `ring` (the workspace's provider), safe default
  protocol versions (hyperd negotiates 1.3).

### Divergences from libpq

| Area | libpq | Here | Why |
|---|---|---|---|
| Default mode | `prefer` | `Disable` | hyperd is usually a local engine with TLS off; an SSLRequest round trip on every connect buys nothing there. |
| `Prefer` on handshake/verification failure | retries without TLS | error | Same as sqlx/tokio-postgres; a downgrade after a failed handshake is exactly what an attacker wants. |
| `VerifyFull` without a root cert | error unless `sslrootcert=system` (PG16+) | bundled webpki roots | Matches the PG16 `system` rule's intent; OS-installed corporate CAs are not included, use `root_cert`. |
| Default cert files (`~/.postgresql/root.crt`, ...) | read | not read | Explicit configuration only. |
| TLS keys in connection strings | accepted | rejected with a pointer to `.tls()` | Keeps one configuration path. |
| `Require`/`Verify*` over a local socket | ignored | `FeatureNotSupported` | A Windows named pipe can be remote; silent plaintext is worse than an error. |

The `TlsMode` rustdoc carries a short form of this table.

## Negotiation

On a freshly connected TCP socket (nodelay, 4 MiB buffers and keepalive
applied first, as today), before `StartupMessage`:

1. Set read/write timeouts on the socket: `Config::connect_timeout` for a
   connection, a fixed `CANCEL_TLS_TIMEOUT` (10 s) for a cancel. Cleared after a
   successful connection handshake; the cancel socket keeps them.
2. Write the 8-byte SSLRequest (`frontend::ssl_request`).
3. Read **exactly one byte** from the raw socket, with no buffered reader in
   between, so nothing the server sends after the reply can be taken as
   plaintext (CVE-2021-23214 class). `RawConnection` is only built afterwards.
4. `'S'`: run the handshake. Sync: `rustls::ClientConnection`, driving
   `complete_io` until `is_handshaking()` is false, then `StreamOwned`. Async:
   `tokio_rustls::TlsConnector::connect`.
5. `'N'`: plaintext on the same socket only if the caller allows it
   (`Fallback::AllowPlaintext`, used by `Prefer` connections); otherwise
   `Error::Tls`. Cancel always passes `Fallback::Deny`.
6. Any other byte (e.g. `'E'`) is `Error::Protocol`.

Error classification: `io::Error`s that wrap a `rustls::Error` (rustls reports
handshake and alert failures that way) become `Error::Tls`, everywhere I/O
errors are converted — not only during negotiation. That matters because under
TLS 1.3 the server's rejection of a *client* certificate arrives after the
client considers the handshake complete, on the first read during startup.
Server-certificate errors surface at connect; client-certificate rejection
surfaces during startup, still as `Error::Tls`. Plain I/O failures (reset,
timeout) stay connection/I-O errors.

An EOF without `close_notify` (`UnexpectedEof` from rustls) is reported as a
clean EOF (`Ok(0)`) by the TLS read arms, so a server dying mid-session maps to
the same `Closed`/connection error as plaintext. PG-wire message framing
already detects truncation.

## Client changes

- `SyncStream::Tls(Box<StreamOwned<ClientConnection, TcpStream>>)` and
  `AsyncStream::Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>)`. New
  `is_tls()`. The unused `SyncStream::try_clone` is deleted.
- `Client` / `AsyncClient` keep `tls: Option<Arc<TlsConnector>>` (built
  `ClientConfig` + `ServerName<'static>`), set **only** when TLS was negotiated
  (a `Prefer` fallback stores `None`), so cancel reuses it without reloading PEM
  files.
- One shared cancel sender per I/O flavour:
  `send_cancel_sync(addr, pid, secret, Option<&TlsConnector>)` and
  `send_cancel_async(..)`. `Client::cancel` and `AsyncClient::cancel_sync`
  (called from `Drop`, must stay blocking and runtime-free) use the sync one;
  `AsyncClient::cancel` uses the async one. Over TLS they write the
  CancelRequest, flush, send `close_notify`, shut down the write half and drain
  to EOF with a short timeout (1 s), so the OS does not RST away the cancel
  because unread session tickets were left in the receive buffer.
- `Client::connect_unix` / `connect_named_pipe` (and `connect_endpoint`'s IPC
  branches) reject `Require`/`VerifyCa`/`VerifyFull` with
  `FeatureNotSupported`; `Prefer`/`Disable` connect in plaintext.

## Testing

Two layers.

**Fake-server unit tests** (`tls.rs` `#[cfg(test)]`, all platforms): a
`std::net::TcpListener` thread that reads 8 bytes and asserts they are an
SSLRequest, answers `S`/`N`/`E`, runs a rustls `ServerConnection`, and records
what it decrypts. They prove what goes on the wire, which hyperd cannot (it
accepts plaintext cancels). Cases: each mode × right/wrong CA, wrong hostname,
`server_name` override, IP host, `'N'` with each fallback, `'E'`, `Prefer` +
wrong CA must error (no downgrade), `Disable` sends no SSLRequest, mTLS
offered/required, handshake timeout against a server that accepts and never
answers, and the cancel senders: SSLRequest, then ClientHello, then a decrypted
16-byte CancelRequest with the right pid/secret; `'N'` on a cancel is
`Error::Tls` and nothing plaintext is sent.

**Real hyperd** (`hyperdb-api/tests/tls_tests.rs`, all platforms; key chmod 0600
on Unix only): rcgen CA + server cert (SANs `localhost`, `127.0.0.1`) with the
certificate file holding `leaf + CA`.

- Enforcement control: plaintext connect to an `ssl_force` server fails. This
  is what makes every other TLS success meaningful.
- `Require` sync and async: `is_tls()`, `Inserter` COPY, a multi-chunk query.
- `VerifyCa` right/wrong CA; `VerifyFull` `localhost` and `127.0.0.1`;
  `server_name("wrong.example")` fails under `VerifyFull` but passes under
  `VerifyCa`; `VerifyCa` without root cert is `Config`.
- `Prefer` against TLS and non-TLS servers; `Prefer` + wrong CA errors;
  `Require` against non-TLS errors.
- mTLS: client cert from the CA accepted; client cert from another CA rejected
  with `Error::Tls`.
- Cancel under TLS + `ssl_force`, sync and async: SQLSTATE 57014.
- Async builder, async pool, sync pool.
- gRPC + `.tls(Require)` is `FeatureNotSupported` (checked before connecting);
  UDS endpoint + `Require` is `FeatureNotSupported`, + `Prefer` connects.
- `Config::from_str` rejects `sslmode=`.

Node smoke test: CA + leaf generated with `openssl` (two invocations; leaf has
`basicConstraints=CA:FALSE`, SAN `localhost`/`127.0.0.1`), started via
`new HyperProcess(undefined, { parameters })` with `ssl_force`; plaintext
`Connection.connect` fails, `Require` and `VerifyFull` + `tlsRootCert(ca)`
succeed with `isTls`. Skipped with a note when `openssl` is missing, except
under `CI`, where it fails.

## Docs

Fix the over-claims to describe what now exists (TCP only, builders and pools
only, the modes table): README.md, AGENTS.md (62, 123), core `lib.rs` and
`client/mod.rs`, DEVELOPMENT.md (42, 92, 271-281, 800, 890),
`hyperdb-api-core/docs/DEVELOPMENT-client.md` (179-189 TLS internals, 233-255
TLS tests, 300-301), `docs/NODEJS_API_SUMMARY.md` (28, 67), the Node README.
Per-crate CHANGELOGs under the right Keep-a-Changelog headings (core: Added /
Changed / Removed, including the new `client::Error::Tls` variant breaking
exhaustive matches; api: Added, plus Changed for the new `tls` field on the pool
configs; node: Added).
