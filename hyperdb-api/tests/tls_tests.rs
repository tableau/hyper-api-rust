// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! TLS against a real `hyperd` started with `ssl_key` / `ssl_certificate`.
//!
//! Most servers here run with `ssl_force`, which makes hyperd reject any
//! plaintext startup. [`plaintext_is_rejected_by_a_force_server`] proves that,
//! and it is what makes every other successful connection evidence of TLS
//! rather than of a silent downgrade.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hyperdb_api::pool::{PoolConfig, SyncPoolConfig, create_pool};
use hyperdb_api::{
    AsyncConnectionBuilder, AsyncInserter, Connection, ConnectionBuilder, CreateMode, Error,
    HyperProcess, Inserter, SqlType, TableDefinition, TlsConfig, TlsMode, TransportMode,
};
use tempfile::TempDir;

/// Rows inserted by the `Require` round-trip tests; enough for several
/// COPY buffers and several result chunks.
const ROWS: i64 = 100_000;

/// Scans 4e10 rows: about 11 s on an Apple M3 Max, where hyperd counts 4e9
/// rows in 1.15 s. That is far past the 500 ms at which the cancel tests fire. If the cancel is lost, the
/// query completes and the test fails instead of hanging.
const SLOW_QUERY: &str = "SELECT count(*) FROM generate_series(1, 200000) a(i), \
                          generate_series(1, 200000) b(i) WHERE a.i + b.i > 3";

/// When the cancel tests fire, relative to the query's start.
const CANCEL_AFTER: Duration = Duration::from_millis(500);

/// A CA, the server certificate it signs, and two client certificates: one
/// from the same CA (accepted by hyperd) and one from an unrelated CA
/// (rejected).
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let ca = generate_ca("hyperdb-api test CA");
        let other_ca = generate_ca("unrelated test CA");
        let server = generate_leaf(&ca, &["localhost", "127.0.0.1"]);
        let client = generate_leaf(&ca, &[]);
        let other_client = generate_leaf(&other_ca, &[]);

        let write = |name: &str, contents: &str| {
            std::fs::write(dir.path().join(name), contents).expect("write fixture file");
        };
        write("ca.pem", &ca.cert_pem);
        write("other_ca.pem", &other_ca.cert_pem);
        // hyperd also uses this file as its client trust store, so it holds
        // the CA after the leaf; that is what lets it verify `client.pem`.
        write("server.pem", &format!("{}{}", server.cert_pem, ca.cert_pem));
        write("server.key", &server.key_pem);
        write("client.pem", &client.cert_pem);
        write("client.key", &client.key_pem);
        write("other_client.pem", &other_client.cert_pem);
        write("other_client.key", &other_client.key_pem);
        restrict_to_owner(&dir.path().join("server.key"));

        Fixture { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

struct TestCa {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    cert_pem: String,
}

struct TestCert {
    cert_pem: String,
    key_pem: String,
}

fn generate_ca(name: &str) -> TestCa {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    let key_pair = rcgen::KeyPair::generate().expect("CA key");
    let cert = params.self_signed(&key_pair).expect("self-sign CA");
    TestCa {
        cert_pem: cert.pem(),
        issuer: rcgen::Issuer::new(params, key_pair),
    }
}

/// A leaf signed by `ca`. `sans` may mix DNS names and IP literals.
fn generate_leaf(ca: &TestCa, sans: &[&str]) -> TestCert {
    let sans = sans.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    let params = rcgen::CertificateParams::new(sans).expect("leaf params");
    let key_pair = rcgen::KeyPair::generate().expect("leaf key");
    let cert = params.signed_by(&key_pair, &ca.issuer).expect("sign leaf");
    TestCert {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    }
}

/// hyperd refuses a key file that group or other can read. The check is
/// POSIX-only, so Windows needs nothing.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod key");
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) {}

/// Starts a TCP hyperd serving `fixture`'s server certificate. With `force`,
/// it rejects every plaintext startup.
fn tls_hyper(name: &str, fixture: &Fixture, force: bool) -> HyperProcess {
    let mut params = common::test_hyper_params(name).expect("test params");
    params.set_transport_mode(TransportMode::Tcp);
    params.set("ssl_key", fixture.path("server.key").to_string_lossy());
    params.set(
        "ssl_certificate",
        fixture.path("server.pem").to_string_lossy(),
    );
    if force {
        params.set("ssl_force", "true");
    }
    HyperProcess::new(None, Some(&params)).expect("start TLS hyperd")
}

fn plain_hyper(name: &str) -> HyperProcess {
    let mut params = common::test_hyper_params(name).expect("test params");
    params.set_transport_mode(TransportMode::Tcp);
    HyperProcess::new(None, Some(&params)).expect("start hyperd")
}

fn endpoint(hyper: &HyperProcess) -> String {
    hyper.require_endpoint().expect("endpoint").to_string()
}

/// `hyper`'s port behind `host`, so a test picks the host name or the IP
/// literal regardless of which one `require_endpoint` reports.
fn endpoint_on(hyper: &HyperProcess, host: &str) -> String {
    let endpoint = endpoint(hyper);
    let (_, port) = endpoint
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("no port in {endpoint}"));
    format!("{host}:{port}")
}

fn verify_ca(fixture: &Fixture) -> TlsConfig {
    TlsConfig::new(TlsMode::VerifyCa).root_cert(fixture.path("ca.pem"))
}

fn verify_full(fixture: &Fixture) -> TlsConfig {
    TlsConfig::new(TlsMode::VerifyFull).root_cert(fixture.path("ca.pem"))
}

fn connect(endpoint: &str, tls: impl Into<TlsConfig>) -> hyperdb_api::Result<Connection> {
    ConnectionBuilder::new(endpoint).tls(tls).build()
}

fn assert_tls_error<T: std::fmt::Debug>(result: hyperdb_api::Result<T>) -> String {
    match result {
        Err(Error::Tls(message)) => message,
        other => panic!("expected Error::Tls, got {other:?}"),
    }
}

/// Inserts [`ROWS`] rows through COPY, then reads them back as several chunks.
fn round_trip_sync(conn: &Connection) {
    let table = TableDefinition::new("tls_rows").add_required_column("id", SqlType::big_int());
    conn.execute_command(&table.to_create_sql(true).expect("create sql"))
        .expect("create table");
    let mut inserter = Inserter::new(conn, &table).expect("inserter");
    for id in 0..ROWS {
        inserter.add_i64(id).expect("add");
        inserter.end_row().expect("end row");
    }
    assert_eq!(
        inserter.execute().expect("COPY"),
        u64::try_from(ROWS).unwrap()
    );

    let mut rowset = conn
        .execute_query("SELECT id FROM tls_rows ORDER BY id")
        .expect("query");
    let (mut seen, mut chunks) = (0_i64, 0);
    while let Some(chunk) = rowset.next_chunk().expect("chunk") {
        chunks += 1;
        for row in &chunk {
            assert_eq!(row.get::<i64>(0), Some(seen));
            seen += 1;
        }
    }
    assert_eq!(seen, ROWS);
    assert!(chunks > 1, "expected a multi-chunk result, got {chunks}");
}

#[test]
fn plaintext_is_rejected_by_a_force_server() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_force_rejects_plaintext", &fixture, true);

    let err = connect(&endpoint(&hyper), TlsMode::Disable).expect_err("plaintext must fail");
    assert!(err.to_string().contains("SSL"), "got {err}");
}

#[test]
fn require_sync_round_trip() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_require_sync", &fixture, true);
    let db = common::test_result_path("tls_require_sync", "hyper").unwrap();

    let conn = ConnectionBuilder::new(endpoint(&hyper))
        .database(&db)
        .create_mode(CreateMode::CreateAndReplace)
        .tls(TlsMode::Require)
        .build()
        .expect("connect over TLS");
    assert!(conn.is_tls());
    round_trip_sync(&conn);
}

#[tokio::test]
async fn require_async_round_trip() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_require_async", &fixture, true);
    let db = common::test_result_path("tls_require_async", "hyper").unwrap();

    let conn = AsyncConnectionBuilder::new(endpoint(&hyper))
        .database(&db)
        .create_mode(CreateMode::CreateAndReplace)
        .tls(TlsMode::Require)
        .build()
        .await
        .expect("connect over TLS");
    assert!(conn.is_tls());

    let table = TableDefinition::new("tls_rows").add_required_column("id", SqlType::big_int());
    conn.execute_command(&table.to_create_sql(true).expect("create sql"))
        .await
        .expect("create table");
    let mut inserter = AsyncInserter::new(&conn, &table).expect("inserter");
    for id in 0..ROWS {
        inserter.add_i64(id).expect("add");
        inserter.end_row().await.expect("end row");
    }
    assert_eq!(
        inserter.execute().await.expect("COPY"),
        u64::try_from(ROWS).unwrap()
    );

    let mut rowset = conn
        .execute_query("SELECT id FROM tls_rows ORDER BY id")
        .await
        .expect("query");
    let (mut seen, mut chunks) = (0_i64, 0);
    while let Some(chunk) = rowset.next_chunk().await.expect("chunk") {
        chunks += 1;
        for row in &chunk {
            assert_eq!(row.get::<i64>(0), Some(seen));
            seen += 1;
        }
    }
    assert_eq!(seen, ROWS);
    assert!(chunks > 1, "expected a multi-chunk result, got {chunks}");
}

#[test]
fn verify_ca_checks_the_issuer() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_verify_ca", &fixture, true);
    let endpoint = endpoint(&hyper);

    let conn = connect(&endpoint, verify_ca(&fixture)).expect("right CA");
    assert!(conn.is_tls());

    let wrong = TlsConfig::new(TlsMode::VerifyCa).root_cert(fixture.path("other_ca.pem"));
    assert_tls_error(connect(&endpoint, wrong));

    match connect(&endpoint, TlsMode::VerifyCa) {
        Err(Error::Config(_)) => {}
        other => panic!("VerifyCa without a root certificate: expected Config, got {other:?}"),
    }
}

#[test]
fn verify_full_checks_the_host_name() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_verify_full", &fixture, true);

    // The certificate has both as SANs: a DNS name and an IP address.
    for endpoint in [
        endpoint_on(&hyper, "localhost"),
        endpoint_on(&hyper, "127.0.0.1"),
    ] {
        let conn = connect(&endpoint, verify_full(&fixture))
            .unwrap_or_else(|err| panic!("VerifyFull against {endpoint}: {err}"));
        assert!(conn.is_tls());
    }

    let endpoint = endpoint(&hyper);
    assert_tls_error(connect(
        &endpoint,
        verify_full(&fixture).server_name("wrong.example"),
    ));
    // VerifyCa skips the host-name check, so the same override passes.
    let conn = connect(&endpoint, verify_ca(&fixture).server_name("wrong.example"))
        .expect("VerifyCa ignores the host name");
    assert!(conn.is_tls());
}

#[test]
fn prefer_uses_tls_when_offered() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_prefer_tls_server", &fixture, true);

    let conn = connect(&endpoint(&hyper), TlsMode::Prefer).expect("Prefer");
    assert!(conn.is_tls());
}

#[test]
fn prefer_falls_back_to_plaintext_without_a_certificate() {
    let hyper = plain_hyper("tls_prefer_plain_server");

    let conn = connect(&endpoint(&hyper), TlsMode::Prefer).expect("Prefer");
    assert!(!conn.is_tls());
}

/// A failed verification under `Prefer` is an error, not a reason to retry
/// in plaintext: only a server that declines TLS is a fallback.
#[test]
fn prefer_does_not_downgrade_on_a_verification_failure() {
    let fixture = Fixture::new();
    // Without ssl_force, a plaintext retry would succeed, so this would pass
    // only if the client never attempted one.
    let hyper = tls_hyper("tls_prefer_wrong_ca", &fixture, false);

    let prefer = TlsConfig::new(TlsMode::Prefer).root_cert(fixture.path("other_ca.pem"));
    assert_tls_error(connect(&endpoint(&hyper), prefer));
}

#[test]
fn require_fails_against_a_server_without_tls() {
    let hyper = plain_hyper("tls_require_plain_server");

    let message = assert_tls_error(connect(&endpoint(&hyper), TlsMode::Require));
    assert!(message.contains("does not support TLS"), "got {message}");
}

#[test]
fn client_certificate_from_the_trusted_ca_is_accepted() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_mtls_accepted", &fixture, true);

    let tls =
        verify_full(&fixture).client_cert(fixture.path("client.pem"), fixture.path("client.key"));
    let conn = connect(&endpoint(&hyper), tls).expect("client certificate from the CA");
    assert!(conn.is_tls());
    let one: i64 = conn.fetch_scalar("SELECT 1").expect("query");
    assert_eq!(one, 1);
}

#[test]
fn client_certificate_from_another_ca_is_rejected() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_mtls_rejected", &fixture, true);

    let tls = verify_full(&fixture).client_cert(
        fixture.path("other_client.pem"),
        fixture.path("other_client.key"),
    );
    // Under TLS 1.3 the rejection can arrive after the handshake, on the
    // first read; the builder's startup exchange is that read.
    assert_tls_error(connect(&endpoint(&hyper), tls));
}

/// hyperd accepts a cancel request in plaintext too, even with `ssl_force`,
/// so this proves only that cancelling a TLS connection works. That the
/// request itself goes over TLS is covered by the fake-server tests in
/// `hyperdb_api_core::client::tls`.
#[test]
fn cancel_under_tls_sync() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_cancel_sync", &fixture, true);

    let conn = Arc::new(connect(&endpoint(&hyper), TlsMode::Require).expect("connect"));
    assert!(conn.is_tls());

    let query_conn = Arc::clone(&conn);
    let start = Instant::now();
    let query = thread::spawn(move || query_conn.fetch_scalar::<i64, _>(SLOW_QUERY));
    thread::sleep(CANCEL_AFTER);
    conn.cancel().expect("send cancel over TLS");

    let err = query
        .join()
        .expect("query thread")
        .expect_err("the slow query must be cancelled");
    assert_eq!(err.sqlstate(), Some("57014"), "got {err}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
}

/// See [`cancel_under_tls_sync`] for what this does and does not prove.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_under_tls_async() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_cancel_async", &fixture, true);

    let conn = AsyncConnectionBuilder::new(endpoint(&hyper))
        .tls(TlsMode::Require)
        .build()
        .await
        .expect("connect");
    assert!(conn.is_tls());

    let start = Instant::now();
    let (result, cancel) = tokio::join!(conn.fetch_scalar::<i64, _>(SLOW_QUERY), async {
        tokio::time::sleep(CANCEL_AFTER).await;
        conn.cancel().await
    });
    cancel.expect("send cancel over TLS");
    let err = result.expect_err("the slow query must be cancelled");
    assert_eq!(err.sqlstate(), Some("57014"), "got {err}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
}

/// Every connection the pool opens is verified TLS, including the first
/// one, which creates the database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pool_connects_over_tls() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_async_pool", &fixture, true);
    let db = common::test_result_path("tls_async_pool", "hyper").unwrap();

    let config = PoolConfig::new(endpoint(&hyper), db.to_string_lossy())
        .create_mode(CreateMode::CreateAndReplace)
        .tls(verify_full(&fixture))
        .max_size(2);
    let pool = create_pool(config).expect("pool");

    let (first, second) = tokio::join!(pool.get(), pool.get());
    for conn in [first.expect("first"), second.expect("second")] {
        assert!(conn.is_tls());
        let one: i64 = conn.fetch_scalar("SELECT 1").await.expect("query");
        assert_eq!(one, 1);
    }
    assert_eq!(pool.status().size, 2);
}

#[test]
fn sync_pool_connects_over_tls() {
    let fixture = Fixture::new();
    let hyper = tls_hyper("tls_sync_pool", &fixture, true);
    let db = common::test_result_path("tls_sync_pool", "hyper").unwrap();

    let pool = SyncPoolConfig::new(endpoint(&hyper), db.to_string_lossy())
        .create_mode(CreateMode::CreateAndReplace)
        .tls(verify_full(&fixture))
        .max_size(2)
        .build();

    let first = pool.get().expect("first");
    let second = pool.get().expect("second");
    for conn in [&first, &second] {
        assert!(conn.is_tls());
        let one: i64 = conn.fetch_scalar("SELECT 1").expect("query");
        assert_eq!(one, 1);
    }
    assert_eq!(pool.status().size, 2);
}

/// Pools open connections lazily, so a TLS failure surfaces from `get`.
#[tokio::test]
async fn pools_report_tls_failures_from_get() {
    let hyper = plain_hyper("tls_pool_plain_server");

    let pool = create_pool(PoolConfig::new(endpoint(&hyper), "unused").tls(TlsMode::Require))
        .expect("pool");
    assert_tls_error(pool.get().await.map(|_| ()));

    let pool = SyncPoolConfig::new(endpoint(&hyper), "unused")
        .tls(TlsMode::Require)
        .build();
    assert_tls_error(pool.get().map(|_| ()));
}

/// gRPC TLS is chosen by the `https://` scheme; `tls()` is rejected before
/// any connection attempt, so the unreachable port is never dialled.
#[test]
fn tls_on_grpc_is_rejected() {
    let err = ConnectionBuilder::new("http://localhost:1")
        .tls(TlsMode::Require)
        .build()
        .expect_err("gRPC + tls must fail");
    assert!(matches!(err, Error::FeatureNotSupported(_)), "got {err:?}");
}

#[cfg(unix)]
#[test]
fn ipc_rejects_required_tls_and_ignores_prefer() {
    let mut params = common::test_hyper_params("tls_ipc").expect("test params");
    params.set_transport_mode(TransportMode::Ipc);
    let hyper = HyperProcess::new(None, Some(&params)).expect("start IPC hyperd");
    // The socket path. `require_endpoint` keeps the descriptor's `/domain/`
    // separator, which is not a filesystem path.
    let endpoint = hyper.connection_endpoint_string().expect("IPC endpoint");
    assert!(endpoint.starts_with('/'), "not a socket path: {endpoint}");

    for mode in [TlsMode::Require, TlsMode::VerifyCa, TlsMode::VerifyFull] {
        match connect(&endpoint, mode) {
            Err(Error::FeatureNotSupported(_)) => {}
            other => {
                panic!("{mode} over a Unix socket: expected FeatureNotSupported, got {other:?}")
            }
        }
    }
    let conn = connect(&endpoint, TlsMode::Prefer).expect("Prefer over a Unix socket");
    assert!(!conn.is_tls());
}
