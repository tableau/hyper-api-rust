# PG-wire TLS Implementation Plan

**Goal:** Implement TLS for PG-wire TCP connections (libpq `sslmode` names, sqlx/tokio-postgres behaviour where libpq differs) in the core sync and async clients, cancel requests, the `hyperdb-api` builders and pools, and the Node bindings, and make the docs match.

**Spec:** [`docs/superpowers/specs/2026-10-10-pg-wire-tls-design.md`](../specs/2026-10-10-pg-wire-tls-design.md). It holds the API, the mode table, the divergences from libpq, the negotiation rules, and the measured hyperd facts. Read it first and do not re-derive any of it.

**Revision:** this is the plan after review. Two reviewers checked it against the code and against the rustls 0.23.45 and tokio-rustls 0.26.6 sources. Each verdict was "with fixes", with no Critical findings, and every fix is folded in below.

**Tech stack:** rustls 0.23 (`ring`, `std`, `tls12`; already a core dependency), tokio-rustls 0.26, webpki-roots 1.0, rcgen 0.14 (core dev-dependency; to be added to `hyperdb-api` dev-dependencies), napi-rs.

## Global constraints

- **Engine and settings.**
  - Use the pinned hyperd only, with `HYPERD_PATH="$PWD/.hyperd/current"` as an absolute path.
  - Start TLS servers through `Parameters::set` with `"ssl_key"`, `"ssl_certificate"` and `"ssl_force"`. These hidden settings are defined in `hyper-db/hyper/tools/hyperd/hyperd.cpp`. Use no others.
- **Code rules.**
  - No narrowing integer `as` casts (AGENTS.md reminder 7).
  - Library code returns errors; it does not panic.
  - Lifetimes get descriptive names, never `'s`.
- **Commits.**
  - Stage files with an explicit `git add <files>`. Never stage `.agents/` or `.codex/`.
  - Use Conventional Commits, with `!` on the breaking iterations.
- **MSRV 1.88.** `cargo +1.88 check --workspace --all-targets` must stay clean.
- **Gate for every Rust iteration:**
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace`
  - `cargo test -p hyperdb-api-core --doc` (the rewritten module example must compile)
  - the iteration's tests, with real output captured.
- **Dead-code lint.** It is on and enforced through `-D warnings`. Crate-private items that have no caller until a later iteration get `#[expect(dead_code, reason = "wired in iteration N")]`. The iteration that adds the caller removes the `expect`, because an unfulfilled `expect` is itself an error.

## Iteration 1: core TLS module, fake-server proven (no client wiring yet)

**Files:**

- `hyperdb-api-core/src/client/tls.rs` (rewrite)
- `hyperdb-api-core/src/protocol/message/frontend.rs` (`ssl_request`)
- `hyperdb-api-core/src/client/error.rs` (the `Tls` variant, `from_io` classification, and the message-accessor match at about line 251)
- `hyperdb-api/src/error.rs` (the mapping, and the `Tls` doc wording)
- `hyperdb-api-core/src/client/config.rs` (`with_tls` and `tls`, plus rejecting libpq TLS keys)
- `hyperdb-api-core/tests/tls_tests.rs` (deleted; its cases move into unit tests)

**Steps:**

1. **`ssl_request`.** Add `frontend::ssl_request(buf)`, which writes `00 00 00 08 04 D2 16 2F`. Unit-test those exact bytes next to `cancel_request`'s test.

2. **Core `Error::Tls`.**
   - Add `client::Error::Tls(String)` with a `tls()` constructor. Handle it in every exhaustive match in core (including the accessor near `error.rs:251`).
   - Map it to `hyperdb_api::Error::Tls(chain)`.
   - Reword the `hyperdb_api::Error::Tls` doc to "TLS negotiation, handshake or certificate-verification failure".
   - In `Error::from_io`, check `err.get_ref().and_then(|e| e.downcast_ref::<rustls::Error>())`. When the inner error is a rustls error the result is `Error::Tls`; otherwise it stays `Io` as today.
   - Add a unit test for each mapping.

3. **`TlsMode`, `ParseTlsModeError` and `TlsConfig`** (per the spec).
   - `TlsMode` is `#[non_exhaustive]` with `Default` = `Disable`, and implements `FromStr` and `Display` with the libpq strings.
   - `FromStr` has `type Err = ParseTlsModeError`. That is a public struct implementing `Display` and `std::error::Error`; its message names the five valid values.
   - The `TlsMode` rustdoc includes the short divergences table.
   - `TlsConfig` has private fields and the setters `new`, `root_cert`, `client_cert` and `server_name`. The path setters take `impl AsRef<Path>` and store a `PathBuf`.
   - `mode()` is public. The other getters are `pub(crate)`.
   - Implement `From<TlsMode> for TlsConfig`.

4. **`TlsConnector`** (crate-private).

   ```rust
   pub(crate) struct TlsConnector { config: Arc<rustls::ClientConfig>, server_name: ServerName<'static> }
   impl TlsConnector {
       /// `Ok(None)` for `Disable`.
       pub(crate) fn build(tls: &TlsConfig, host: &str) -> Result<Option<Arc<Self>>>;
   }
   ```

   - **PEM loading.** Use `rustls::pki_types::pem::PemObject`: `CertificateDer::pem_file_iter` and `PrivateKeyDer::from_pem_file`. A load error is `Error::Config` naming the file. A file that loads but contains zero certificates is `Error::Config` too.
   - **Verifiers, per the spec table:**
     - `NoVerify` delegates `verify_tls12_signature`, `verify_tls13_signature` and `supported_verify_schemes` to `provider.signature_verification_algorithms`.
     - `ChainOnly { roots, provider }` implements `verify_server_cert` by calling `rustls::client::verify_server_cert_signed_by_trust_anchor(&ParsedCertificate::try_from(end_entity)?, &roots, intermediates, now, algs.all)`. It does not match on name errors. Signature methods delegate as above.
     - `VerifyFull` uses the stock `WebPkiServerVerifier::builder_with_provider(roots, provider)`.
     - Roots are `root_cert` if it is set, and otherwise (`VerifyFull` only) `webpki_roots::TLS_SERVER_ROOTS`.
   - `VerifyCa` without `root_cert` is `Error::Config`.
   - Client auth uses `with_client_auth_cert` when `client_cert` is set, and `with_no_client_auth` otherwise.
   - **Server name.** Take the `server_name` override, or else `host` with `[`/`]` stripped, and pass it to `ServerName::try_from(..).to_owned()`. Invalid input is `Error::Config`.
   - `#[expect(dead_code)]` on anything not yet called outside tests.

5. **Negotiation and cancel senders** (crate-private, in `tls.rs`).

   ```rust
   pub(crate) enum Fallback { AllowPlaintext, Deny }
   pub(crate) enum Negotiated<Plain, Tls> { Plain(Plain), Tls(Tls) }
   pub(crate) fn negotiate_sync(tcp: TcpStream, c: &TlsConnector, fallback: Fallback, timeout: Duration)
       -> Result<Negotiated<TcpStream, StreamOwned<ClientConnection, TcpStream>>>;
   pub(crate) async fn negotiate_async(tcp: tokio::net::TcpStream, c: &TlsConnector, fallback: Fallback, timeout: Duration)
       -> Result<Negotiated<tokio::net::TcpStream, tokio_rustls::client::TlsStream<tokio::net::TcpStream>>>;
   pub(crate) fn send_cancel_sync(addr: &str, process_id: i32, secret_key: i32, tls: Option<&TlsConnector>) -> Result<()>;
   pub(crate) async fn send_cancel_async(addr: &str, process_id: i32, secret_key: i32, tls: Option<&TlsConnector>) -> Result<()>;
   ```

   - **Negotiation.**
     - Sync: `set_read_timeout` and `set_write_timeout` to `timeout`, write the SSLRequest, then `read_exact(&mut [0u8; 1])` on the bare socket.
       - On `'S'`, loop `while conn.is_handshaking() { conn.complete_io(&mut tcp)?; }`, then clear the timeouts and return `StreamOwned::new`.
       - On `'N'`, use `Fallback`: plaintext when it is allowed, `Error::Tls("server does not support TLS")` otherwise.
       - Any other byte is `Error::Protocol`.
     - Async: the whole negotiation runs under `tokio::time::timeout(timeout, ..)`, and an elapsed timeout is `Error::Timeout`.
     - Error classification follows `from_io` (rustls errors become `Tls`; I/O errors and timeouts stay `Io`/`Timeout`).
   - **Cancel senders.**
     - Match the existing cancel code exactly for socket setup: connect, then `set_nodelay`.
     - With `tls` set, negotiate with `Fallback::Deny` and a timeout of `CANCEL_TLS_TIMEOUT` (10 s). Write the CancelRequest (`frontend::cancel_request`) and flush.
     - Then send `close_notify`. Sync: `conn.send_close_notify()`, `complete_io`, then `shutdown(Write)`. Async: `shutdown().await`.
     - Then drain to EOF with a 1 s timeout and ignore the outcome.
     - Without `tls`, the plaintext behaviour is the same as today. The async plaintext path gains a `flush().await`.
     - The sync sender must not need a tokio runtime, because it runs from `Drop`.

6. **`Config` changes.**
   - Add `Config::with_tls(impl Into<TlsConfig>)` and `Config::tls() -> &TlsConfig`, defaulting to `Disable`. The docs say TCP only, `Prefer` is plaintext over IPC, and the other modes are rejected over IPC.
   - `Config::from_str` rejects the keys `sslmode`, `sslrootcert`, `sslcert`, `sslkey`, `sslpassword`, `sslcrl`, `sslsni` and `sslnegotiation` with `Error::Config("TLS options are not accepted in the connection string; use ConnectionBuilder::tls")`. Unit-test it.

7. **Remove the old API.** Delete the public `rustls_impl` and the old helpers. The new module doc example compiles (it is not `ignore`) and has no `TODO`.

8. **Fake-server unit tests** (`#[cfg(test)] mod tests` in `tls.rs`; rcgen is already a core dev-dependency).
   - The harness is a `std::net::TcpListener` thread. It reads 8 bytes and asserts they are an SSLRequest, replies `S`, `N` or `E`, and on `S` runs a rustls `ServerConnection` with an optional client verifier. It reports the first 16 decrypted bytes (or the plaintext bytes after `N`) back over a channel.
   - Cases:
     - **Require:**
       - with an unknown CA and no root cert: OK;
       - with the wrong root cert: `Tls` (chain-only applies under Require too).
     - **VerifyCa:**
       - right CA: OK;
       - wrong CA: `Tls`;
       - wrong hostname: OK;
       - no root cert: `Config`.
     - **VerifyFull:**
       - right host: OK;
       - wrong hostname: `Tls`;
       - matching `server_name` override: OK;
       - IP host `127.0.0.1` with an IP SAN: OK.
     - **Prefer:**
       - against `S` with a wrong CA: `Tls`, not plaintext (no downgrade);
       - against `N`: plaintext.
     - **Server replies `N` or `E`:**
       - Require against `N`: `Tls`;
       - `E`: `Protocol`.
     - **Disable:** `TlsConnector::build` returns `None`.
     - **mTLS:**
       - server requires a client cert and one from the CA is offered: OK;
       - none offered: `Tls` on the first read after the handshake (TLS 1.3 delivers the alert late). Drive one read.
     - **Timeout:** a server that accepts the connection and never answers makes `negotiate_sync` fail within the timeout. Use 500 ms in the test.
     - **`send_cancel_sync` against `S`:**
       - the server observes the SSLRequest, then a handshake, then a decrypted CancelRequest with code 80877102 and the right pid and secret;
       - against `N`, the result is `Tls` and the server receives nothing after the SSLRequest.
     - **`send_cancel_async`:** the same pair of cases.
     - **`negotiate_async`:** one Require case and one VerifyFull case.
     - **Parsing:** `TlsMode` parse and display round-trip; an unknown string gives `ParseTlsModeError`, whose message names all five valid modes.
     - **PEM files:** a missing file or an empty one is `Config`.
     - **`Config::from_str("h:1/db?sslmode=require")`** is `Config`.
   - Delete `hyperdb-api-core/tests/tls_tests.rs` once its cases exist here.

**Acceptance:**

- The gate passes.
- `cargo test -p hyperdb-api-core --lib` passes with its output captured.
- No client behaviour changes yet, except two that are intended: `sslmode=` in a connection string is now rejected, and rustls I/O errors are classified as TLS.

**Commit:** `feat(core)!: TLS negotiation module with sslmode verifiers and TLS cancel sender`

## Iteration 2: core clients and builders, sync and async

**Files:**

- core: `sync_stream.rs`, `async_stream.rs`, `client.rs`, `async_client.rs`
- `hyperdb-api`: `connection_builder.rs`, `async_connection_builder.rs`, `connection.rs`, `async_connection.rs`, `transport.rs` (as needed for `is_tls`), `lib.rs`, `Cargo.toml` (rcgen dev-dependency)
- new: `tests/tls_tests.rs`

**Steps:**

1. **Stream types.**
   - Add `SyncStream::Tls(Box<StreamOwned<ClientConnection, TcpStream>>)`, delegating `Read` and `Write`.
     - The Tls `Read` arm maps `io::ErrorKind::UnexpectedEof` to `Ok(0)`; see the spec for why.
     - Socket-option methods go through `.sock`.
     - Delete the unused `try_clone`.
   - Add `AsyncStream::Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>)`, delegating `poll_read` (with the same `UnexpectedEof` → EOF mapping, i.e. return `Ready(Ok(()))` with nothing filled), `poll_write`, `poll_write_vectored`, `is_write_vectored`, `poll_flush` and `poll_shutdown`.
   - Add `is_tls()` on both.
2. **TCP connect** (`Client::connect`, `AsyncClient::connect`).
   - Apply the socket options as today.
   - If `TlsConnector::build(config.tls(), config.host())?` is `Some(c)`, call `negotiate_*` with `Fallback::AllowPlaintext` only when the mode is `Prefer`, and with `config.connect_timeout()`.
   - Wrap the result in the matching stream variant.
   - Store `tls: Option<Arc<TlsConnector>>` only when TLS was negotiated.
   - Add `is_tls()` accessors.
   - Remove iteration 1's `expect(dead_code)` on items that now have callers.
3. **IPC.** In `connect_unix` and `connect_named_pipe` (sync and async; this also covers `connect_endpoint`'s IPC branches), return `FeatureNotSupported("TLS is only available over TCP; use a host:port endpoint")` when the mode is Require, VerifyCa or VerifyFull. Prefer and Disable connect in plaintext.
4. **Cancel.** The TCP branches of `Client::cancel` and `AsyncClient::cancel_sync` call `send_cancel_sync(addr, pid, secret, self.tls.as_deref())`. `AsyncClient::cancel` calls `send_cancel_async`. The IPC branches are unchanged.
5. **`hyperdb-api` wiring.**
   - Re-export `TlsConfig`, `TlsMode` and `ParseTlsModeError` at the crate root.
   - Both builders get a `tls: TlsConfig` field and `pub fn tls(mut self, tls: impl Into<TlsConfig>) -> Self`. Its docs cover:
     - TCP only;
     - IPC behaviour;
     - gRPC is rejected;
     - a short form of the modes table;
     - the static `connect*` shortcuts and `Connection::new(&hyper, ..)` stay plaintext.
   - `build_tcp`, `build_unix` and `build_named_pipe` set `config.with_tls(self.tls.clone())`.
   - `build_grpc` checks TLS **first**, before connecting and next to the `query_timeout` check: any mode other than `Disable` returns `FeatureNotSupported("TLS for gRPC is selected by the https:// scheme")`.
   - `Connection::is_tls()` and `AsyncConnection::is_tls()` return `true` when the TCP client negotiated TLS or the gRPC config `is_tls()`.
6. **Test fixture** (a module inside `tls_tests.rs`).
   - rcgen generates a CA, a server leaf (SANs `localhost` and `127.0.0.1`), a client leaf from the same CA, and a second CA with its own client leaf, all in a `TempDir`.
   - The server certificate file is `leaf PEM + CA PEM` (that file is also hyperd's client trust store).
   - On Unix only, `chmod 0600` the key.
   - `fn tls_hyper(name: &str, force: bool) -> (HyperProcess, Fixture)` starts from `common::test_hyper_params(name)` (check its exact shape), adds the ssl parameters, and sets `TransportMode::Tcp` explicitly.
7. **Tests** in `hyperdb-api/tests/tls_tests.rs`:
   - **Enforcement control.** A plaintext builder against a `force` server fails, and the error mentions SSL.
   - **Require, sync and async.**
     - `is_tls()` is true.
     - Create a table, insert 100k rows (`Inserter`; async `AsyncInserter` or `AsyncArrowInserter`), then `SELECT` all of them across more than one chunk and check the count.
   - **VerifyCa:** right CA OK; wrong CA `Tls`.
   - **VerifyFull:**
     - `localhost` OK, and `127.0.0.1:PORT` OK;
     - `server_name("wrong.example")` gives `Tls`, while VerifyCa with the same override is OK.
   - **VerifyCa without a root cert** is `Config`.
   - **Prefer:**
     - against a TLS server, `is_tls()` is true;
     - against a plain `HyperProcess`, the connection works and `!is_tls()`;
     - with the wrong CA against a TLS server, `Tls`.
   - **Require against a non-TLS server** is `Tls`.
   - **mTLS:** a client cert from the CA is OK; one from the other CA gives `Tls`. The rejection may arrive during startup; assert on the variant.
   - **Cancel under TLS + force, sync and async.**
     - Follow `connection_tests.rs:280-370`.
     - First, in this iteration, measure the query: `SELECT count(*) FROM generate_series(1,200000) a(i), generate_series(1,200000) b(i) WHERE a.i + b.i > 3`. Keep at least a 5× margin over the cancel delay, and grow the series if it is fast.
     - Cancel after about 500 ms and assert SQLSTATE 57014 strictly.
     - A comment explains that hyperd also accepts a plaintext cancel, so the proof that the cancel goes over TLS is the fake-server test from iteration 1; this test proves the TLS cancel path works end to end.
   - **gRPC:** an `http://localhost:1` endpoint with `.tls(TlsMode::Require)` gives `FeatureNotSupported`, with no server needed.
   - **Unix only:** a UDS endpoint (`HyperProcess` with `TransportMode::Ipc`) with Require gives `FeatureNotSupported`; with Prefer it connects and `!is_tls()`.
   - **Red-before-green.** Stash the `with_tls` line in `build_tcp`. Require against the force server must then fail (plaintext rejected). Capture both runs.

**Acceptance:**

- The gate passes.
- `cargo test -p hyperdb-api --test tls_tests`, `-p hyperdb-api-core`, and `-p hyperdb-api --test connection_tests --test async_connection_tests` (check the file names) pass, with output captured.

**Commit:** `feat(api,core)!: TLS for TCP connections via the builders' tls(), cancel over TLS`

## Iteration 3: pools

**Files:** `hyperdb-api/src/pool.rs` and `hyperdb-api/tests/tls_tests.rs`.

**Steps:**

1. Add `pub tls: TlsConfig` (default `Disable`) to `PoolConfig` and `SyncPoolConfig`, with a `pub fn tls(mut self, impl Into<TlsConfig>) -> Self`. Include it in their `Debug`.
2. Route both `open`s through the builders **unconditionally**:
   - set endpoint, database and mode;
   - set user and password only when **both** are `Some`, as today;
   - pass `.tls(self.config.tls.clone())`.
   This gives one code path. The async pool with auth and a non-TCP endpoint used to fail through `connect_with_auth`; this fixes it as a side effect, so add a test if it is cheap.
3. Tests:
   - A sync pool and an async pool with Require against a force server: check out two connections, run a query on each, and check `is_tls()` on both.
   - The existing `pool_tests` must stay green.

**Acceptance:** the gate passes, and `pool_tests` and `tls_tests` pass.

**Commit:** `feat(api)!: TLS option on PoolConfig and SyncPoolConfig; pools connect through the builders`

## Iteration 4: Node bindings

**Files:** `hyperdb-api-node/src/types.rs`, `src/connection.rs`, `src/process.rs`, `index.d.ts`, `__test__/smoke.mjs`, `README.md`, and `docs/NODEJS_API_SUMMARY.md`.

**Steps:**

1. Add a `#[napi(string_enum)] pub enum TlsMode { Disable, Prefer, Require, VerifyCa, VerifyFull }` and implement `From<TlsMode> for hyperdb_api::TlsMode`.
2. Builder:
   - Fields: `tls_mode: Option<TlsMode>`, `tls_root_cert`, `tls_client_cert: Option<(String, String)>` and `tls_server_name`.
   - Setters: `tlsMode`, `tlsRootCert`, `tlsClientCert(certPath, keyPath)` and `tlsServerName`, following the existing `&mut self -> &Self` pattern.
   - `build()`: when a sub-option is set without `tlsMode`, return an error. When a mode is set, assemble a `TlsConfig`.
3. Add a `#[napi(getter)] is_tls`, which is `isTls` in JS.
4. `HyperProcess`: change the constructor to `new(hyper_path: Option<String>, options: Option<HyperProcessOptions>)`. `#[napi(object)] HyperProcessOptions { parameters: Option<HashMap<String, String>> }` is applied with `Parameters::set`. Existing one-argument calls keep working.
5. Update `index.d.ts` (hand-written): the `TlsMode` const enum in CreateMode style, the builder methods, the `isTls` getter, `HyperProcessOptions`, and the constructor.
6. Smoke test section:
   - Locate `openssl` and run `openssl version`. If it is missing, skip with a printed note, but **fail** when `process.env.CI` is set.
   - Generate a CA, then a leaf signed by it with `basicConstraints=critical,CA:FALSE`, `extendedKeyUsage=serverAuth` and `subjectAltName=DNS:localhost,IP:127.0.0.1`, using an extfile (portable across OpenSSL and LibreSSL, so no `-addext`).
   - Write the cert file as leaf plus CA, `chmod 0600` the key, and start `new HyperProcess(undefined, { parameters: { ssl_key, ssl_certificate, ssl_force: 'true' } })`.
   - Plaintext `Connection.connect` fails.
   - Require: `conn.isTls === true`, and a query works.
   - VerifyFull with `tlsRootCert(ca)`: OK.
   - `tlsRootCert` without `tlsMode`: `build()` rejects.
7. Run `npm run build:debug && npm test` in `hyperdb-api-node/` with an absolute `HYPERD_PATH` and capture the output. This audit effort has never run the Node suite, so record any pre-existing failure unrelated to TLS separately instead of fixing it here.

**Acceptance:** the Node build and `npm test` pass with output captured, and `index.d.ts` agrees with the napi exports (spot-check names).

**Commit:** `feat(node): TLS options on ConnectionBuilder, isTls, HyperProcess options`

## Iteration 5: docs, changelogs, MCP note, full run

**Steps:**

1. Fix the doc over-claims at the spec's listed lines. Then run `rg -in '\btls\b|\bssl' --glob '*.md' --glob '*.rs' --glob '*.d.ts'` and fix any remaining claim.
   - **README.md:** the TCP/TLS line.
   - **AGENTS.md:** lines 62 and 123.
   - **Core:** `lib.rs:14` and `client/mod.rs:24,256-259`.
   - **DEVELOPMENT.md:** lines 42, 92, 271-281, 800 and 890.
   - **`hyperdb-api-core/docs/DEVELOPMENT-client.md`:**
     - rewrite `## TLS Internals` (179-189) for the new module;
     - rewrite `### TLS Tests` (233-255) for the new unit and integration tests;
     - fix 300-301.
   - **`docs/NODEJS_API_SUMMARY.md`:** lines 28 and 67.
   - **Node README:** the builder section.
2. Per-crate CHANGELOG `## [Unreleased]`, merged into existing headings (MD024):
   - **`hyperdb-api-core`:**
     - Added: negotiation, `Config::with_tls`, `ParseTlsModeError`, `Error::Tls`. The new variant breaks exhaustive matches, because core's error type is deliberately not `#[non_exhaustive]`.
     - Changed: `TlsMode::VerifyCA` → `VerifyCa`; `TlsConfig::new(mode)` and private fields; `Config::from_str` rejects `ssl*` keys; rustls I/O errors are classified as `Tls`.
     - Removed: `rustls_impl`, `danger_accept_invalid_certs`, `has_client_cert`, `verify_server`, the `TlsMode` helper methods, `SyncStream::try_clone`.
   - **`hyperdb-api`:**
     - Added: `tls()` on the builders and pools, `is_tls`, the re-exports.
     - Changed: the new `tls` field on `PoolConfig` and `SyncPoolConfig` breaks struct-literal construction; pools connect through the builders; Require, VerifyCa and VerifyFull over IPC are `FeatureNotSupported`.
   - **`hyperdb-api-node`:** Added. The `HyperProcess` options expose hyperd parameters to JS, mirroring the public Rust `Parameters::set`.
3. MCP deferral comment at the `AttachSource` "Future: Tcp" note in `hyperdb-mcp/src/attach.rs`: a future `kind:"tcp"` must accept `tls_mode`, `tls_root_cert` (plus the client cert and server name) and pass them to `ConnectionBuilder::tls`. This is a comment only; the tool surface does not change.
4. Run `npx markdownlint-cli2` and judge its findings against the pre-existing backlog.
5. Full gate:
   - fmt, clippy, doc, doctests;
   - `cargo +1.88 check --workspace --all-targets`;
   - `HYPERD_PATH="$PWD/.hyperd/current" cargo test --workspace --no-fail-fast`, with the `test result` lines summed.
6. Update ssteiner-ai `STATUS.md` and **do not commit it**. Record:
   - decision 8 done, with the commits;
   - the MCP deferral;
   - the pre-existing finding that `Config::connect_timeout` and `login_timeout` are not applied to the TCP connect.

**Commit:** `docs: describe the real TLS support; changelogs; MCP TLS note`

## Review checkpoints

- **After each iteration:** a `feature-dev:code-reviewer` pass on the diff (pasted inline), briefed with the spec and that iteration's acceptance criteria.
- **Final:** `feature-dev:code-reviewer` and `code-review` in parallel on the whole range. Focus:
  - the single-byte read;
  - `Fallback` use: Prefer only on connect, never on cancel;
  - the verifier per mode;
  - the cancel close sequence and timeouts, including `cancel_sync` from `Drop`;
  - error classification;
  - the IPC rejection;
  - no narrowing casts;
  - doc accuracy.
